#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'socket'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: socket_path_length_e2e.rb SERVER EVIDENCE [--reject-only]' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
reject_only = ARGV.include?('--reject-only')

# Exec preserves the child's PID, so each fixture can hit an exact pathname
# byte length despite different server PID widths. This still runs production
# MCP over stdio, with no internal-function calls or server test switches.
launcher = <<~'LAUNCHER'
  root, target_bytes, encoding, binary = ARGV
  suffix = "/kwin-mcp-#{Process.pid}-1/service_bus_socket"
  remaining = Integer(target_bytes) - root.bytesize - 1 - suffix.bytesize
  raise 'fixture root too long' unless remaining.positive?
  component = encoding == 'utf8' ? ('é' * (remaining / 2) + 'x' * (remaining % 2)) : 'x' * remaining
  ENV['TMPDIR'] = File.join(root, component)
  Dir.mkdir(ENV.fetch('TMPDIR'))
  exec binary, '--autoclean', '--width', '800', '--height', '600'
LAUNCHER

# Deny all host services, including wallet access, during positive startup
# checks. The upstream is only used for the host environment validity check.
Dir.mktmpdir('k353-', '/tmp') do |root|
  home = File.join(evidence, 'home')
  %w[.config .cache .local/state .local/share .kde].each { |name| FileUtils.mkdir_p(File.join(home, name)) }
  proxy_path = File.join(root, 'host-bus')
  proxy_log = File.open(File.join(evidence, 'host-proxy.stderr'), 'w')
  proxy_pid = Process.spawn('xdg-dbus-proxy', ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=/run/user/#{Process.euid}/bus"), proxy_path, '--filter', out: File::NULL, err: proxy_log)
  begin
    Timeout.timeout(5) { sleep 0.01 until File.socket?(proxy_path) }
    env = {
      'HOME' => home,
      'XDG_CONFIG_HOME' => File.join(home, '.config'),
      'XDG_CACHE_HOME' => File.join(home, '.cache'),
      'XDG_STATE_HOME' => File.join(home, '.local/state'),
      'XDG_DATA_HOME' => File.join(home, '.local/share'),
      'KDEHOME' => File.join(home, '.kde'),
      'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
      'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{proxy_path}",
      'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
      'XAUTHORITY' => nil,
      'KWIN_MCP_REPO' => File.join(root, 'no-repository'),
      'CARGO' => '/usr/bin/false',
    }
    cases = [['long-ascii', 180, 'ascii'], ['linux-one-over', 108, 'ascii'], ['linux-boundary', 107, 'ascii'],
             ['dbus-one-over', 100, 'ascii'], ['long-utf8', 100, 'utf8']]
    cases += [['dbus-boundary', 99, 'ascii'], ['short', 80, 'ascii']] unless reject_only
    cases.each do |name, bytes, encoding|
      directory = File.join(evidence, name)
      FileUtils.mkdir_p(directory)
      case_root = File.join(root, name)
      Dir.mkdir(case_root)
      File.open(File.join(directory, 'server.stderr'), 'w') do |diagnostic|
        Open3.popen3(env, 'ruby', '-e', launcher, case_root, bytes.to_s, encoding, binary) do |input, output, error, process|
          stderr_reader = Thread.new { IO.copy_stream(error, diagnostic) }
          suffix = "/kwin-mcp-#{process.pid}-1/service_bus_socket"
          remaining = bytes - case_root.bytesize - 1 - suffix.bytesize
          component = encoding == 'utf8' ? ('é' * (remaining / 2) + 'x' * (remaining % 2)) : 'x' * remaining
          tmpdir = File.join(case_root, component)
          workdir = File.join(tmpdir, "kwin-mcp-#{process.pid}-1")
          socket = File.join(workdir, 'service_bus_socket')
          raise 'wrong fixture byte length' unless socket.bytesize == bytes
          raise 'UTF-8 fixture does not distinguish bytes from characters' if encoding == 'utf8' && socket.length >= 100
          id = 0
          trace = File.open(File.join(directory, 'rpc.jsonl'), 'w')
          rpc = lambda do |method, params|
            id += 1
            request = {jsonrpc: '2.0', id: id, method: method, params: params}
            trace.puts(JSON.generate(request)); trace.flush
            input.puts(JSON.generate(request)); input.flush
            Timeout.timeout(45) do
              loop do
                line = output.gets or raise 'unexpected server EOF'
                reply = JSON.parse(line)
                trace.puts(JSON.generate(reply)); trace.flush
                break reply if reply['id'] == id
              end
            end
          end
          result = lambda do |response|
            raise response.inspect if response['error'] || response.dig('result', 'isError')
            response.fetch('result')
          end
          call = lambda { |tool, arguments = {}| rpc.call('tools/call', {name: tool, arguments: arguments}) }
          begin
            result.call(rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'socket-path-proof', version: '1'}}))
            input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
            started_at = Process.clock_gettime(Process::CLOCK_MONOTONIC)
            response = call.call('session_start')
            elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started_at
            if bytes > 99
              message = response.dig('error', 'message') || response.dig('result', 'content')&.map { |item| item['text'] }.join(' ')
              raise "expected path-length rejection, got #{response.inspect}" unless response['error'] || response.dig('result', 'isError')
              fragments = ['TMPDIR', 'bytes', bytes >= 109 ? 'sockaddr_un.sun_path' : 'libdbus pathname limit of 99 bytes']
              fragments.each do |fragment|
                raise "missing #{fragment}: #{response.inspect}" unless message&.include?(fragment)
              end
              rejected = bytes == 100 ? socket : File.join(workdir, 'system_bus_socket')
              raise "missing offending path/byte count: #{message}" unless message.include?(rejected) && message.include?("#{rejected.bytesize} bytes")
              raise "proxy timeout concealed validation (#{elapsed}s)" if elapsed >= 3
              raise 'failed startup created runtime workdir' if File.exist?(workdir)
              raise 'failed startup created disk twin' if File.exist?(File.join(home, '.cache', 'kwin-mcp', File.basename(workdir)))
              raise 'failed startup left a session' unless result.call(call.call('session_list')).dig('structuredContent', 'sessions') == []
              # Failure leaves the server usable, and the next generated name
              # receives the same validation before acquiring any resources.
              retry_response = call.call('session_start')
              retry_message = retry_response.dig('error', 'message').to_s
              raise 'retry did not validate the next session path' unless retry_message.include?("kwin-mcp-#{process.pid}-2/") && retry_message.include?('TMPDIR')
              raise 'retry created runtime resources' unless Dir.children(tmpdir).empty?
            else
              started = result.call(response).fetch('structuredContent')
              session_id = started.fetch('session_id')
              sessions = result.call(call.call('session_list')).dig('structuredContent', 'sessions')
              record = sessions.find { |session| session['session_id'] == session_id }
              raise 'workdir mismatch' unless record && record['workdir'] == workdir
              raise 'proxy did not bind at the requested byte boundary' unless File.socket?(socket)
              sandbox_pid = record.fetch('sandbox_pid')
              raise 'sandbox absent' unless File.directory?("/proc/#{sandbox_pid}")
              result.call(call.call('window_list', {session_id: session_id}))
              result.call(call.call('session_stop', {session_id: session_id}))
              raise 'stopped sandbox remains' if File.directory?("/proc/#{sandbox_pid}")
              raise 'stopped workdir remains' if File.exist?(workdir)
              raise 'stopped disk twin remains' if File.exist?(File.join(home, '.cache', 'kwin-mcp', File.basename(workdir)))
              raise 'stopped session remains listed' unless result.call(call.call('session_list')).dig('structuredContent', 'sessions') == []
            end
            File.write(File.join(directory, 'verification.json'), JSON.pretty_generate(status: 'passed', socket_bytes: bytes, socket_characters: socket.length, socket: socket, elapsed: elapsed))
            puts "#{name}=PASS (#{bytes} bytes, #{elapsed.round(3)}s)"
          ensure
            input.close
            begin
              status = Timeout.timeout(15) { process.value }
            rescue Timeout::Error
              Process.kill('TERM', process.pid)
              status = Timeout.timeout(15) { process.value }
            end
            stderr_reader.join
            trace.close
            raise "server exit #{status.exitstatus}" unless status.success?
          end
        end
      end
    end
  ensure
    Process.kill('TERM', proxy_pid) rescue Errno::ESRCH
    Process.wait(proxy_pid)
    proxy_log.close
  end
end
