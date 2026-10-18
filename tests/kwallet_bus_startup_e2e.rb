#!/usr/bin/env ruby
# Exercise the production stdio server and real dbus-daemon startup failures.
# Requires the normal session_start host dependencies, including /dev/uinput.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary = ARGV.fetch(0) { abort 'usage: kwallet_bus_startup_e2e.rb /path/to/kwin-mcp' }
binary = File.realpath(binary)
daemon = ENV.fetch('PATH').split(':').map { |path| File.join(path, 'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon

def rpc(input, output, id, method, params = {})
  input.puts(JSON.generate({jsonrpc: '2.0', id: id, method: method, params: params}))
  input.flush
  Timeout.timeout(60) do
    loop do
      line = output.gets or raise 'unexpected server EOF'
      response = JSON.parse(line)
      return response if response['id'] == id
    end
  end
end

def result(response)
  raise response.inspect if response['error'] || response.dig('result', 'isError')
  response.fetch('result').fetch('structuredContent')
end

def gone(pid)
  !File.exist?("/proc/#{pid}")
end

Dir.mktmpdir('kw364-', '/tmp') do |root|
  wrapper_dir = File.join(root, 'bin')
  FileUtils.mkdir_p(wrapper_dir)
  File.write(File.join(wrapper_dir, 'dbus-daemon'), <<~'BASH')
    #!/usr/bin/env bash
    set -eu
    config=
    for arg in "$@"; do
      case "$arg" in --config-file=*) config=${arg#--config-file=} ;; esac
    done
    test -n "$config"
    # Record the daemon and the already-owned system proxy independently of
    # the MCP response so the harness can check cleanup after failure.
    printf '%s\n' "$$" > "$KW364_CASE/daemon.pid"
    cat "/proc/$PPID/task/$PPID/children" > "$KW364_CASE/children"
    printf '%s\n' "$config" > "$KW364_CASE/config"
    case "$KW364_MODE" in
      invalid|noisy)
        printf '<busconfig><broken></busconfig>\n' > "$config"
        if test "$KW364_MODE" = noisy; then
          printf 'diagnostic-prefix\377\n' >&2
          head -c 131072 /dev/zero | tr '\000' x >&2
          printf '\377\n' >&2
        fi
        ;;
      timeout)
        # The real daemon blocks opening this FIFO, without creating a socket.
        rm "$config"
        mkfifo "$config"
        # A regular file at the expected name is not a ready bus socket.
        touch "${config%/*}/kwallet_bus"
        printf 'waiting for fixture configuration\n' >&2
        ;;
      normal) ;;
    esac
    exec "$KW364_DAEMON" "$@"
  BASH
  File.chmod(0o755, File.join(wrapper_dir, 'dbus-daemon'))

  # An empty private upstream bus prevents wallet access to the host desktop.
  bus_socket = File.join(root, 'upstream')
  bus_error = File.open(File.join(root, 'upstream.stderr'), 'w')
  bus_pid = Process.spawn(daemon, '--session', '--nofork', '--nopidfile', "--address=unix:path=#{bus_socket}", out: File::NULL, err: bus_error)
  begin
    Timeout.timeout(5) { sleep 0.01 until File.socket?(bus_socket) }
    %w[invalid noisy timeout normal].each do |mode|
      directory = File.join(root, mode)
      home = File.join(directory, 'home')
      FileUtils.mkdir_p(home)
      environment = {
        'HOME' => home,
        'XDG_CONFIG_HOME' => File.join(home, '.config'),
        'XDG_DATA_HOME' => File.join(home, '.local/share'),
        'XDG_CACHE_HOME' => File.join(home, '.cache'),
        'XDG_STATE_HOME' => File.join(home, '.local/state'),
        'KDEHOME' => File.join(home, '.kde'),
        'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
        'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{bus_socket}",
        'TMPDIR' => root,
        'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
        'XAUTHORITY' => nil,
        'KWIN_MCP_REPO' => File.join(root, 'no-repository'),
        'CARGO' => '/usr/bin/false',
        'PATH' => "#{wrapper_dir}:#{ENV.fetch('PATH')}",
        'KW364_MODE' => mode, 'KW364_CASE' => directory, 'KW364_DAEMON' => daemon,
      }
      File.open(File.join(directory, 'server.stderr'), 'w') do |diagnostic|
        Open3.popen3(environment, binary, '--autoclean', '--width', '640', '--height', '480') do |input, output, error, process|
          reader = Thread.new { IO.copy_stream(error, diagnostic) }
          session_id = nil
          begin
            initialized = rpc(input, output, 1, 'initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'kwallet-startup-proof', version: '1'}})
            raise initialized.inspect if initialized['error']
            input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush
            response = rpc(input, output, 2, 'tools/call', {name: 'session_start', arguments: {}})
            config = File.read(File.join(directory, 'config')).strip
            elapsed = Time.now - File.mtime(File.join(directory, 'config'))
            if mode == 'normal'
              session_id = result(response).fetch('session_id')
              raise 'private bus socket missing' unless File.socket?(File.join(File.dirname(config), 'kwallet_bus'))
              result(rpc(input, output, 3, 'tools/call', {name: 'window_list', arguments: {session_id: session_id}}))
              result(rpc(input, output, 4, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}}))
              session_id = nil
              puts 'PASS: normal: real KWallet bus and isolated KWin session start, window_list, and stop'
            else
              message = response.dig('error', 'message') || response.dig('result', 'content')&.map { |item| item['text'] }.join(' ')
              raise "expected startup error: #{response.inspect}" unless (response['error'] || response.dig('result', 'isError')) && message
              raise message unless message.include?('KWallet bus') && message.include?('dbus-daemon stderr at')
              if mode == 'timeout'
                raise message unless message.include?('within 3s') && message.include?('waiting for fixture configuration')
                raise "timeout was not bounded: #{elapsed}" unless elapsed.between?(2.8, 6)
              else
                raise message unless message.include?('exited with exit status: 1')
                raise "early exit waited for timeout: #{elapsed}" unless elapsed < 2.5
                if mode == 'invalid'
                  raise message unless message.include?('Failed to start message bus:') && message.include?('<broken>')
                else
                  raise message unless message.include?('diagnostic-prefix�') && message.include?('[truncated]')
                  raise 'unbounded diagnostic response' unless message.bytesize < 18000
                end
              end
              puts "PASS: #{mode}: actionable error in #{format('%.3f', elapsed)}s after daemon launch"
            end
            raise 'unpublished session remained' unless result(rpc(input, output, 5, 'tools/call', {name: 'session_list', arguments: {}})).fetch('sessions') == []
            pids = File.read(File.join(directory, 'children')).split.map(&:to_i)
            daemon_pid = Integer(File.read(File.join(directory, 'daemon.pid')))
            raise 'system proxy was not observed' unless pids.include?(daemon_pid) && pids.length >= 2
            Timeout.timeout(5) { sleep 0.02 until pids.all? { |pid| gone(pid) } }
            raise 'autoclean left the startup workdir' if File.exist?(File.dirname(config))
            puts "PASS: #{mode}: daemon and prior proxy reaped; session and temporary workdir removed"
          ensure
            rpc(input, output, 10, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}}) if session_id
            input.close
            status = Timeout.timeout(15) { process.value }
            reader.join
            unless status.success?
              raise "server exit #{status.inspect}: #{File.read(File.join(directory, 'server.stderr'))}"
            end
          end
        end
      end
    end
  ensure
    Process.kill('TERM', bus_pid) unless gone(bus_pid)
    Process.wait(bus_pid)
    bus_error.close
  end
end
