#!/usr/bin/env ruby
# Control sockets (#597): sessions start while other kwin-mcp control sockets
# appear and disappear, and a process inside a session cannot use a server's
# control socket even though it is no longer masked.
require 'fileutils'
require 'json'
require 'open3'
require 'socket'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: control_socket_isolation_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.uid}")

# Fake control sockets that come and go, as other servers start and exit.
churn_stop = false
churn = Thread.new do
  n = 0
  until churn_stop
    path = File.join(runtime, "kwin-mcp-control-#{4_000_000 + (n % 50)}.sock")
    begin
      server = UNIXServer.new(path)
      sleep 0.02
      server.close
    rescue SystemCallError
      nil
    ensure
      File.unlink(path) if File.socket?(path)
    end
    n += 1
  end
end

Dir.mktmpdir('k597-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
  env = {
    'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config", 'XDG_CACHE_HOME' => "#{home}/.cache",
    'XDG_DATA_HOME' => "#{home}/.local/share", 'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
    'XDG_RUNTIME_DIR' => runtime,
    'DBUS_SESSION_BUS_ADDRESS' => ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime}/bus"),
    'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil, 'XAUTHORITY' => nil,
    'KWIN_MCP_REPO' => "#{home}/no-source", 'CARGO' => '/usr/bin/false'
  }
  trace = File.open(File.join(evidence, 'rpc.jsonl'), 'w')
  input, output, process = Open3.popen2(env, binary, '--autoclean', err: File.join(evidence, 'server.stderr'))
  id = 0
  rpc = lambda do |method, params|
    id += 1
    request = {jsonrpc: '2.0', id: id, method: method, params: params}
    trace.puts(JSON.generate(request: request)); trace.flush
    input.puts(JSON.generate(request)); input.flush
    response = Timeout.timeout(120) { loop { value = JSON.parse(output.gets || raise('server EOF')); break value if value['id'] == id } }
    trace.puts(JSON.generate(response: response)); trace.flush
    response
  end
  call = ->(name, args = {}) { rpc.call('tools/call', {name: name, arguments: args}) }
  sessions = []
  begin
    rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'control-socket', version: '1'}})
    input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
    5.times do |attempt|
      started = call.('session_start', {browser_profiles: 'fresh', width: 800, height: 600})
      raise "start #{attempt + 1} failed during socket churn: #{started['error'] || started}" if started['error'] || started.dig('result', 'isError')
      sessions << started.dig('result', 'structuredContent', 'session_id')
    end
    churn_stop = true
    churn.join
    own = File.join(runtime, "kwin-mcp-control-#{process.pid}.sock")
    raise "server control socket missing: #{own}" unless File.socket?(own)
    # From inside the session, reach the server's control socket through the
    # read-only runtime alias and try an MCP initialize.
    probe = 'for s in /run/kwin-mcp-host-sockets/*/kwin-mcp-control-' + process.pid.to_s + '.sock; do ' \
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"inside\",\"version\":\"1\"}}}' " \
            "| timeout 5 socat -t2 - UNIX-CONNECT:\"$s\"; echo \"path=$s status=$?\"; done"
    launched = call.('launch_app', {session_id: sessions.first, command: probe, expect_window: false})
    stdout_path = launched.dig('result', 'structuredContent', 'stdout_path') || raise("no stdout_path: #{launched}")
    reply = Timeout.timeout(20) { loop { text = File.exist?(stdout_path) ? File.read(stdout_path) : ''; break text if text.include?('status='); sleep 0.2 } }
    File.write(File.join(evidence, 'inside-probe.txt'), reply)
    raise "control socket not visible in the session: #{reply}" unless reply.include?("kwin-mcp-control-#{process.pid}.sock")
    raise "a session process got an MCP reply from the control socket: #{reply}" if reply.include?('"result"')
    rejected = File.read(File.join(evidence, 'server.stderr')).include?("rejected peer outside this server's PID namespace")
    raise 'server did not log the namespace rejection' unless rejected
    puts "PASS: #{sessions.size} starts during socket churn; in-session control access rejected"
  ensure
    churn_stop = true
    churn.join
    sessions.each { |sid| call.('session_stop', {session_id: sid}) rescue nil }
    input.close
    Timeout.timeout(60) { process.value }
    trace.close
  end
end
