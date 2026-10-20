#!/usr/bin/env ruby
# Real production MCP session: compare a HOME overlay socket with shared IPC.
# Usage: ruby tests/shared_ipc_e2e.rb SERVER EVIDENCE_DIR
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'socket'
require 'tmpdir'
require 'timeout'

binary = File.realpath(ARGV.fetch(0))
evidence = File.expand_path(ARGV.fetch(1))
FileUtils.mkdir_p(evidence)

def rpc(method, params)
  @sequence += 1
  @input.puts(JSON.generate(jsonrpc: '2.0', id: @sequence, method: method, params: params))
  @input.flush
  response = Timeout.timeout(60) do
    loop do
      value = JSON.parse(@output.gets || raise('server EOF'))
      break value if value['id'] == @sequence
    end
  end
  @trace.puts(JSON.generate(method: method, params: params, response: response))
  @trace.flush
  raise "MCP failure: #{response}" if response['error'] || response.dig('result', 'isError')
  response.fetch('result')
end

def call(name, arguments = {})
  arguments = arguments.merge(session_id: @session) if @session && name != 'session_list'
  rpc('tools/call', name: name, arguments: arguments).fetch('structuredContent')
end

Dir.mktmpdir('kmipc-', '/var/tmp') do |root|
  home = File.join(root, 'h')
  %w[.config .cache .local/share .local/state .kde].each { |dir| FileUtils.mkdir_p(File.join(home, dir)) }
  environment = {
    'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config",
    'XDG_CACHE_HOME' => "#{home}/.cache", 'XDG_DATA_HOME' => "#{home}/.local/share",
    'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
    'XDG_RUNTIME_DIR' => "/run/user/#{Process.uid}",
    'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{Process.uid}/bus",
    'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
    'KWIN_MCP_REPO' => "#{home}/no-source", 'CARGO' => '/usr/bin/false',
  }
  File.open(File.join(evidence, 'rpc.jsonl'), 'w') do |trace|
    @trace = trace
    File.open(File.join(evidence, 'server.stderr'), 'w') do |diagnostics|
      Open3.popen3(environment, binary, '--autoclean', '--no-viewer') do |input, output, errors, process|
        @input, @output, @sequence = input, output, 0
        reader = Thread.new { IO.copy_stream(errors, diagnostics) }
        begin
          rpc('initialize', protocolVersion: '2025-06-18', capabilities: {}, clientInfo: {name: 'shared-ipc-e2e', version: '1'})
          input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
          input.flush
          started = call('session_start', width: 800, height: 600, browser_profiles: 'fresh')
          @session = started.fetch('session_id')
          shared = started.fetch('workdir')
          entry = call('session_list').fetch('sessions').find { |item| item['session_id'] == @session }
          raise 'session_list lost the live session' unless entry
          shared_socket = File.join(shared, 'fixture-511.sock')
          ready = File.join(shared, 'fixture-511.ready')
          quit = File.join(shared, 'fixture-511.quit')
          script = File.join(shared, 'fixture-511.rb')
          private_marker = "/tmp/kwin-ipc-private-#{Process.pid}"
          payload = "shared-511-#{Process.pid}"
          File.write(script, <<~'LISTENER')
            require 'socket'
            shared, ready, quit, private_marker, payload = ARGV
            overlay = File.join(ENV.fetch('HOME'), 'fixture-511.sock')
            listeners = [UNIXServer.new(overlay), UNIXServer.new(shared)]
            File.write(private_marker, 'private')
            File.write(ready, Process.pid.to_s)
            begin
              until File.exist?(quit)
                readable = IO.select(listeners, nil, nil, 0.1)
                next unless readable
                readable.first.each do |server|
                  client = server.accept
                  begin
                    client.puts(payload)
                  ensure
                    client.close
                  end
                end
              end
            ensure
              listeners.each(&:close)
              [overlay, shared, private_marker, ready].each { |path| File.unlink(path) if File.exist?(path) }
            end
          LISTENER
          launched = call('launch_app', command: ['ruby', script, shared_socket, ready, quit, private_marker, payload].shelljoin, window_timeout_seconds: 1)
          raise 'helper submission not reported' unless launched.fetch('command_submitted')
          Timeout.timeout(10) { sleep 0.02 until File.file?(ready) && File.socket?(shared_socket) }
          received = Timeout.timeout(5) { UNIXSocket.open(shared_socket, &:gets) }.to_s.strip
          raise 'host did not reach the session listener' unless received == payload
          raise 'private /tmp leaked into host /tmp' if File.exist?(private_marker)
          upper = File.join(home, '.cache', 'kwin-mcp', File.basename(shared), 'overlay-upper')
          overlay_socket = File.join(upper, 'fixture-511.sock')
          raise 'HOME overlay socket not found in the owned backing directory' unless File.socket?(overlay_socket)
          # Relative connect avoids the Unix sockaddr path length limit. Merely
          # observing a long absolute path failure would not prove this bug.
          overlay_result = begin
            Dir.chdir(upper) { Timeout.timeout(3) { UNIXSocket.open('fixture-511.sock', &:gets) } }
          rescue Errno::ECONNREFUSED
            'ECONNREFUSED'
          end
          raise "overlay backing path unexpectedly reached a listener: #{overlay_result.inspect}" unless overlay_result == 'ECONNREFUSED'
          File.write(quit, 'stop owned helper')
          Timeout.timeout(10) { sleep 0.02 while File.exist?(ready) || File.exist?(shared_socket) }
          stopped = call('session_stop')
          raise 'owned session did not stop' unless stopped['status'] == 'stopped'
          @session = nil
          raise 'owned workdir survived autoclean' if File.exist?(shared)
          proof = {
            session_start_alias: started['host_shared_dir'] == shared,
            session_list_alias: entry['host_shared_dir'] == shared,
            shared_socket_reply: received, overlay_backing_socket: overlay_result,
            private_tmp_isolated: true, helper_stopped: true, session_cleanup: true,
          }
          File.write(File.join(evidence, 'results.json'), JSON.pretty_generate(proof))
          puts JSON.generate(proof)
          raise 'host_shared_dir missing or inconsistent' unless proof[:session_start_alias] && proof[:session_list_alias]
        ensure
          if @session
            begin
              call('session_stop')
            rescue StandardError => error
              warn "cleanup: #{error}"
            end
          end
          @session = nil
          input.close unless input.closed?
          begin
            Timeout.timeout(20) { process.value }
          rescue Timeout::Error
            Process.kill('TERM', process.pid)
            Timeout.timeout(20) { process.value }
          end
          reader.join(5)
        end
      end
    end
  end
end
