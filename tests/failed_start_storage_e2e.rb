#!/usr/bin/env ruby
# Run the production stdio server with real startup dependencies and an empty
# private HOME/upstream D-Bus. Never use the host wallet or browser profiles.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary = File.realpath(ARGV.fetch(0) { abort 'usage: failed_start_storage_e2e.rb SERVER [MODE ...]' })
modes = ARGV.drop(1)
modes = %w[invalid retry shutdown timeout retained normal] if modes.empty?
abort 'unknown mode' unless (modes - %w[invalid retry shutdown timeout retained normal]).empty?
daemon = ENV.fetch('PATH').split(':').map { |dir| File.join(dir, 'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon

def rpc(input, output, id, method, params = {})
  input.puts(JSON.generate(jsonrpc: '2.0', id: id, method: method, params: params))
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

Dir.mktmpdir('kw369-', '/tmp') do |root|
  wrapper_dir = File.join(root, 'bin')
  FileUtils.mkdir_p(wrapper_dir)
  File.write(File.join(wrapper_dir, 'dbus-daemon'), <<~'BASH')
    #!/usr/bin/env bash
    set -eu
    for arg in "$@"; do
      case "$arg" in --config-file=*) config=${arg#--config-file=} ;; esac
    done
    printf '%s\n' "$config" > "$KW369_CASE/config"
    cat "/proc/$PPID/task/$PPID/children" > "$KW369_CASE/children"
    disk="$XDG_CACHE_HOME/kwin-mcp/$(basename "${config%/*}")"
    test -d "$disk/overlay-upper"
    mkdir -p "$disk/locked/nested"
    printf 'private overlay data\n' > "$disk/locked/nested/payload"
    chmod 000 "$disk/locked/nested" "$disk/locked"
    ln -s "$KW369_CASE/outside" "$disk/outside-link"
    case "$KW369_MODE" in
      invalid|retained|retry|shutdown)
        printf '<busconfig><broken></busconfig>\n' > "$config"
        ;;
      timeout|normal) ;;
    esac
    case "$KW369_MODE" in
      retry|shutdown) chmod 500 "$XDG_CACHE_HOME/kwin-mcp" ;;
    esac
    exec "$KW369_DAEMON" "$@"
  BASH
  File.chmod(0o755, File.join(wrapper_dir, 'dbus-daemon'))
  socket = File.join(root, 'upstream')
  bus_error = File.open(File.join(root, 'upstream.stderr'), 'w')
  bus_pid = Process.spawn(daemon, '--session', '--nofork', '--nopidfile', "--address=unix:path=#{socket}", out: File::NULL, err: bus_error)
  begin
    Timeout.timeout(5) { sleep 0.01 until File.socket?(socket) }
    modes.each do |mode|
      directory = File.join(root, mode)
      home = File.join(directory, 'home')
      outside = File.join(directory, 'outside')
      FileUtils.mkdir_p([home, File.join(outside, 'nested')])
      File.write(File.join(outside, 'nested', 'sentinel'), 'keep me')
      File.chmod(0o500, outside)
      environment = {
        'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
        'XDG_DATA_HOME' => File.join(home, '.local/share'),
        'XDG_CACHE_HOME' => File.join(home, '.cache'),
        'XDG_STATE_HOME' => File.join(home, '.local/state'), 'KDEHOME' => File.join(home, '.kde'),
        'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
        'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{socket}", 'TMPDIR' => root,
        'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil, 'XAUTHORITY' => nil,
        'KWIN_MCP_REPO' => File.join(root, 'no-repository'), 'CARGO' => '/usr/bin/false',
        'PATH' => "#{wrapper_dir}:#{ENV.fetch('PATH')}",
        'KW369_MODE' => mode, 'KW369_CASE' => directory, 'KW369_DAEMON' => daemon,
        # Existing debug hook: cancel a real startup after its sandbox exists.
        'KWIN_MCP_TEST_STARTUP_DELAY_STAGE' => mode == 'timeout' ? 'after-bwrap' : nil,
        'KWIN_MCP_TEST_STARTUP_DELAY_MS' => mode == 'timeout' ? '30000' : nil,
      }
      args = mode == 'retained' ? [] : ['--autoclean']
      File.open(File.join(directory, 'server.stderr'), 'w') do |diagnostic|
        Open3.popen3(environment, binary, *args, '--width', '640', '--height', '480') do |input, output, error, process|
          reader = Thread.new { IO.copy_stream(error, diagnostic) }
          session_id = nil
          disk_root = File.join(home, '.cache', 'kwin-mcp')
          begin
            result_init = rpc(input, output, 1, 'initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'failed-start-storage-proof', version: '1'}})
            raise result_init.inspect if result_init['error']
            input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
            response = rpc(input, output, 2, 'tools/call', {name: 'session_start', arguments: {}})
            config = File.read(File.join(directory, 'config')).strip
            workdir = File.dirname(config)
            disk = File.join(disk_root, File.basename(workdir))
            if mode == 'normal'
              session_id = result(response).fetch('session_id')
              raise 'live session storage missing' unless File.directory?(workdir) && File.directory?(disk)
              result(rpc(input, output, 3, 'tools/call', {name: 'window_list', arguments: {session_id: session_id}}))
              result(rpc(input, output, 4, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}}))
              session_id = nil
            else
              message = response.dig('error', 'message')
              raise "expected startup error: #{response.inspect}" unless message
              expected = mode == 'timeout' ? 'hard limit' : 'KWallet bus'
              raise "wrong startup failure: #{message}" unless message.include?(expected)
            end
            raise 'failed/stopped session still listed' unless result(rpc(input, output, 5, 'tools/call', {name: 'session_list', arguments: {}})).fetch('sessions') == []
            pids = File.read(File.join(directory, 'children')).split.map(&:to_i)
            raise 'startup children not observed' unless pids.length >= 2
            Timeout.timeout(5) { sleep 0.02 until pids.all? { |pid| gone(pid) } }
            if %w[retry shutdown].include?(mode)
              session_id = response.dig('error', 'data', 'session_id')
              raise 'disk cleanup did not retain retry ownership' unless session_id && message.include?(disk) && message.include?('Cleanup is still owned')
              raise 'cleanup failure lost workdir or lease' unless File.directory?(disk) && File.file?(File.join(workdir, '.kwin-mcp-autoclean'))
              stopped = rpc(input, output, 6, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}})
              raise 'stop hid disk removal error' unless stopped.dig('error', 'message')&.include?(disk)
              File.chmod(0o700, disk_root)
              if mode == 'retry'
                raise 'retry did not report cleanup' unless result(rpc(input, output, 7, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}})).fetch('status') == 'cleaned'
                session_id = nil
              end
            end
            unless mode == 'shutdown'
              remains = mode == 'retained'
              raise "temporary workdir retention wrong (#{mode})" unless File.exist?(workdir) == remains
              raise "disk overlay retention wrong (#{mode}): #{disk}" unless File.exist?(disk) == remains
            end
            input.close
            status = Timeout.timeout(20) { process.value }
            reader.join
            raise "server exit #{status.inspect}" unless status.success?
            unless mode == 'retained'
              raise 'shutdown left session storage' if File.exist?(workdir) || File.exist?(disk)
            end
            raise 'cleanup changed symlink target permissions' unless File.stat(outside).mode & 0o777 == 0o500
            raise 'cleanup deleted symlink target data' unless File.read(File.join(outside, 'nested', 'sentinel')) == 'keep me'
            puts "PASS: #{mode}: storage, child cleanup, and outside symlink target verified"
          ensure
            File.chmod(0o700, disk_root) if File.directory?(disk_root)
            unless input.closed?
              rpc(input, output, 99, 'tools/call', {name: 'session_stop', arguments: {session_id: session_id}}) if session_id
              input.close
            end
            Timeout.timeout(25) { process.value }
            reader.join
          end
        end
      end
    end
  ensure
    Process.kill('TERM', bus_pid) unless gone(bus_pid)
    Process.wait(bus_pid)
    bus_error.close
    # Retention and failure cases deliberately leave mode-000 fixture trees.
    FileUtils.chmod_R('u+rwx', root)
  end
end
