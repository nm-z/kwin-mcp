#!/usr/bin/env ruby
# Run the production stdio server with two live desktops. A later session_start
# must not invalidate an earlier session's handle, its listing must match what the
# desktop tools accept, and a stopped handle must answer session_stop as a no-op.
# Uses a private HOME, TMPDIR, and upstream D-Bus, so no host profile or wallet is touched.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary = File.realpath(ARGV.fetch(0) { abort 'usage: session_registry_e2e.rb SERVER' })
daemon = ENV.fetch('PATH').split(':').map { |dir| File.join(dir, 'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon

def rpc(input, output, id, method, params = {})
  input.puts(JSON.generate(jsonrpc: '2.0', id: id, method: method, params: params))
  input.flush
  Timeout.timeout(120) do
    loop do
      line = output.gets or raise 'unexpected server EOF'
      response = JSON.parse(line)
      return response if response['id'] == id
    end
  end
end

def call(input, output, id, name, arguments = {})
  rpc(input, output, id, 'tools/call', name: name, arguments: arguments)
end

def succeeded(response)
  raise response.inspect if response['error'] || response.dig('result', 'isError')
  response.fetch('result')
end

def structured(response)
  succeeded(response).fetch('structuredContent')
end

def gone(pid)
  !File.exist?("/proc/#{pid}")
end

Dir.mktmpdir('kw-registry-', '/tmp') do |root|
  socket = File.join(root, 'upstream')
  bus = Process.spawn(daemon, '--session', '--nofork', '--nopidfile', "--address=unix:path=#{socket}",
                      out: File::NULL, err: File::NULL)
  begin
    Timeout.timeout(5) { sleep 0.01 until File.socket?(socket) }
    home = File.join(root, 'home')
    FileUtils.mkdir_p(home)
    environment = {
      'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
      'XDG_DATA_HOME' => File.join(home, '.local/share'),
      'XDG_CACHE_HOME' => File.join(home, '.cache'),
      'XDG_STATE_HOME' => File.join(home, '.local/state'), 'KDEHOME' => File.join(home, '.kde'),
      'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
      'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{socket}", 'TMPDIR' => root,
      'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil, 'XAUTHORITY' => nil,
      'KWIN_MCP_REPO' => File.join(root, 'no-repository'), 'CARGO' => '/usr/bin/false',
    }
    log = File.join(root, 'server.stderr')
    File.open(log, 'w') do |diagnostic|
      Open3.popen3(environment, binary, '--autoclean', '--width', '640', '--height', '480') do |input, output, error, process|
        reader = Thread.new { IO.copy_stream(error, diagnostic) }
        begin
          rpc(input, output, 1, 'initialize', protocolVersion: '2024-11-05', capabilities: {},
                                             clientInfo: { name: 'session-registry', version: '1' })
          input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
          input.flush

          first = structured(call(input, output, 2, 'session_start', {})).fetch('session_id')
          second = structured(call(input, output, 3, 'session_start', {})).fetch('session_id')
          raise 'session ids collide' if first == second

          listed = structured(rpc(input, output, 4, 'tools/call', name: 'session_list', arguments: {}))
          running = listed.fetch('sessions').select { |session| session['status'] == 'running' }.map { |session| session['session_id'] }
          raise "session_list does not show both live sessions: #{listed}" unless running.sort == [first, second].sort

          # The later start must leave the earlier handle usable: window_list and export both succeed.
          succeeded(call(input, output, 5, 'window_list', session_id: first))
          hostname = File.join(root, 'exported-hostname')
          succeeded(call(input, output, 6, 'export_file', session_id: first,
                                                          session_path: '/etc/hostname', host_path: hostname))
          raise 'export from the earlier session did not copy the file' unless File.file?(hostname)
          succeeded(call(input, output, 7, 'window_list', session_id: second))

          stopped = structured(call(input, output, 8, 'session_stop', session_id: first))
          raise "stop of the earlier session did not report stopped: #{stopped}" unless stopped['status'] == 'stopped'
          again = structured(call(input, output, 9, 'session_stop', session_id: first))
          raise "repeated stop was not a confirmed no-op: #{again}" unless again['status'] == 'already_cleared'
          answer = call(input, output, 10, 'window_list', session_id: first)
          raise "stopped session still answers desktop tools: #{answer}" unless answer['error'] || answer.dig('result', 'isError')

          remaining = structured(rpc(input, output, 12, 'tools/call', name: 'session_list', arguments: {})).fetch('sessions')
          raise "session_list still shows the stopped session: #{remaining}" if remaining.any? { |session| session['session_id'] == first }

          structured(call(input, output, 13, 'session_stop', session_id: second))
          final = structured(rpc(input, output, 14, 'tools/call', name: 'session_list', arguments: {}))
          raise "session_list not empty after stopping every session: #{final}" unless final.fetch('sessions').empty?
          none = structured(call(input, output, 15, 'session_stop', {}))
          raise "stop with nothing running was not a no-op: #{none}" unless none['status'] == 'none'

          puts "session_registry=PASS (#{first} and #{second} live together; export and window_list from the earlier handle; stops confirmed)"
        ensure
          input.close
          Timeout.timeout(60) { process.value }
          reader.join(5)
        end
      end
    end
  ensure
    Process.kill('TERM', bus) unless gone(bus)
    Process.wait(bus)
    FileUtils.chmod_R('u+rwx', root)
  end
end
