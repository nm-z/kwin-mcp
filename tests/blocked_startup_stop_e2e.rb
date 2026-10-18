#!/usr/bin/env ruby
# Run the production stdio server with a host scan blocked past its startup
# budget. session_stop must answer while the blocked call lasts, and clear the
# handle, with its lifecycle record, once the call returns. Uses a private HOME,
# TMPDIR, and upstream D-Bus, so no host profile or wallet is touched.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary = File.realpath(ARGV.fetch(0) { abort 'usage: blocked_startup_stop_e2e.rb SERVER' })
daemon = ENV.fetch('PATH').split(':').map { |dir| File.join(dir, 'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon

BLOCK_MILLISECONDS = 8_000
START_BUDGET_SECONDS = 3

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

def call(input, output, id, name, arguments = {})
  rpc(input, output, id, 'tools/call', name: name, arguments: arguments)
end

def structured(response)
  raise response.inspect if response['error'] || response.dig('result', 'isError')
  response.fetch('result').fetch('structuredContent')
end

def gone(pid)
  !File.exist?("/proc/#{pid}")
end

Dir.mktmpdir('kw-blocked-', '/tmp') do |root|
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
      # Existing debug hook: a thread sleep inside the host scan, which no async timeout preempts.
      'KWIN_MCP_TEST_BLOCK_HOST_SCAN_MS' => BLOCK_MILLISECONDS.to_s,
    }
    log = File.join(root, 'server.stderr')
    File.open(log, 'w') do |diagnostic|
      Open3.popen3(environment, binary, '--autoclean', '--width', '640', '--height', '480') do |input, output, error, process|
        reader = Thread.new { IO.copy_stream(error, diagnostic) }
        begin
          rpc(input, output, 1, 'initialize', protocolVersion: '2024-11-05', capabilities: {},
                                             clientInfo: { name: 'blocked-startup-stop', version: '1' })
          input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
          input.flush

          started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
          failed = rpc(input, output, 2, 'tools/call', name: 'session_start',
                                                   arguments: { startup_timeout_seconds: START_BUDGET_SECONDS })
          elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started
          data = failed.dig('error', 'data') or raise "session_start did not time out: #{failed.inspect}"
          raise "session_start did not report the blocked call: #{data}" unless data['host_call_blocked'] == true
          session_id = data.fetch('session_id')
          raise "timed-out start answered late (#{elapsed.round(1)}s)" if elapsed > START_BUDGET_SECONDS + 4

          listed = structured(rpc(input, output, 3, 'tools/call', name: 'session_list', arguments: {}))
          entry = listed.fetch('sessions').find { |session| session['session_id'] == session_id }
          raise "blocked start not listed: #{listed}" unless entry && entry['status'] == 'start_blocked'

          stop_started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
          pending = structured(call(input, output, 4, 'session_stop', session_id: session_id))
          stop_elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - stop_started
          raise "stop of a blocked start did not answer pending: #{pending}" unless pending['status'] == 'cleanup_pending'
          raise "stop of a blocked start waited on the gate (#{stop_elapsed.round(1)}s)" if stop_elapsed > 2

          cleared = nil
          id = 5
          Timeout.timeout(BLOCK_MILLISECONDS / 1000 + 30) do
            loop do
              response = structured(call(input, output, id += 1, 'session_stop', session_id: session_id))
              if response['status'] == 'cleaned'
                cleared = response
                break
              end
              raise "unexpected stop answer: #{response}" unless response['status'] == 'cleanup_pending'
              sleep 0.5
            end
          end
          record = cleared.fetch('session_lifecycle_record')
          unless record && record['state'] == 'session_start_abandoned' && record['reason'] == 'host_call_blocked' &&
                 record['workdir_cleanup'] == 'removed'
            raise "clear did not record the abandoned start: #{cleared}"
          end

          remaining = structured(rpc(input, output, 90, 'tools/call', name: 'session_list', arguments: {}))
          raise "cleared handle still listed: #{remaining}" unless remaining.fetch('sessions').empty?

          repeated = structured(call(input, output, 91, 'session_stop', session_id: session_id))
          raise "repeated stop was not a confirmed no-op: #{repeated}" unless repeated['status'] == 'already_cleared'

          none = structured(call(input, output, 92, 'session_stop', {}))
          raise "stop with nothing running was not a no-op: #{none}" unless none['status'] == 'none'

          workdirs = Dir.glob(File.join(root, 'kwin-mcp-*'))
          raise "blocked start left a workdir behind: #{workdirs}" unless workdirs.empty?

          puts "blocked_startup_stop=PASS (#{session_id}: start answered in #{elapsed.round(1)}s, stop answered pending in #{stop_elapsed.round(2)}s, cleared with record)"
        ensure
          input.close
          Timeout.timeout(20) { process.value }
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
