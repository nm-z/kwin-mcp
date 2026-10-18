#!/usr/bin/env ruby
# Run the production stdio server with a one-minute idle TTL. An idle desktop must
# expire on its own, leave a lifecycle record, and stay listed as expired until
# session_stop clears it. Uses a private HOME, TMPDIR, and upstream D-Bus.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary = File.realpath(ARGV.fetch(0) { abort 'usage: ttl_expiry_registry_e2e.rb SERVER' })
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

IDLE_POLL_SECONDS = 5
IDLE_LIMIT_SECONDS = 180

Dir.mktmpdir('kw-ttl-', '/tmp') do |root|
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
      Open3.popen3(environment, binary, '--ttl', '1', '--width', '640', '--height', '480') do |input, output, error, process|
        reader = Thread.new { IO.copy_stream(error, diagnostic) }
        begin
          rpc(input, output, 1, 'initialize', protocolVersion: '2024-11-05', capabilities: {},
                                             clientInfo: { name: 'ttl-expiry', version: '1' })
          input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
          input.flush

          session_id = structured(call(input, output, 2, 'session_start', {})).fetch('session_id')
          # session_list does not count as activity, so the desktop stays idle.
          expired = nil
          deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + IDLE_LIMIT_SECONDS
          id = 2
          until expired || Process.clock_gettime(Process::CLOCK_MONOTONIC) > deadline
            sleep IDLE_POLL_SECONDS
            listed = structured(rpc(input, output, id += 1, 'tools/call', name: 'session_list', arguments: {}))
            expired = listed.fetch('sessions').find { |session| session['session_id'] == session_id && session['status'] == 'expired' }
          end
          raise 'idle desktop did not expire under --ttl 1' unless expired

          answer = call(input, output, id += 1, 'window_list', session_id: session_id)
          raise "expired desktop still answers desktop tools: #{answer}" unless answer['error'] || answer.dig('result', 'isError')

          cleared = structured(call(input, output, id += 1, 'session_stop', session_id: session_id))
          record = cleared.fetch('session_lifecycle_record')
          unless cleared['status'] == 'cleaned' && record && record['state'] == 'session_expired'
            raise "session_stop did not clear the expired handle with its record: #{cleared}"
          end

          remaining = structured(rpc(input, output, id += 1, 'tools/call', name: 'session_list', arguments: {}))
          raise "cleared handle still listed: #{remaining}" unless remaining.fetch('sessions').empty?
          workdirs = Dir.glob(File.join(root, 'kwin-mcp-*'))
          raise "expired desktop left a workdir behind: #{workdirs}" unless workdirs.empty?

          puts "ttl_expiry_registry=PASS (#{session_id} expired while idle, listed as expired, cleared with its record)"
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
