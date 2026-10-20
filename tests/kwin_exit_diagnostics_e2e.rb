#!/usr/bin/env ruby
# When kwin_wayland exits during startup, session_start reports that exit and
# its status at once instead of waiting for the Wayland socket timeout (#596).
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: kwin_exit_diagnostics_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)

Dir.mktmpdir('k596-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
  fake = File.join(home, 'fake-bin')
  FileUtils.mkdir_p(fake)
  File.write(File.join(fake, 'kwin_wayland'), "#!/bin/sh\necho 'fake kwin_wayland: refusing to start' >&2\nexit 3\n")
  File.chmod(0o755, File.join(fake, 'kwin_wayland'))
  runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.uid}")
  env = {
    'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config", 'XDG_CACHE_HOME' => "#{home}/.cache",
    'XDG_DATA_HOME' => "#{home}/.local/share", 'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
    'XDG_RUNTIME_DIR' => runtime, 'PATH' => "#{fake}:#{ENV.fetch('PATH')}",
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
    response = Timeout.timeout(90) { loop { value = JSON.parse(output.gets || raise('server EOF')); break value if value['id'] == id } }
    trace.puts(JSON.generate(response: response)); trace.flush
    response
  end
  begin
    rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'kwin-exit', version: '1'}})
    input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
    started_at = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    response = rpc.call('tools/call', {name: 'session_start', arguments: {browser_profiles: 'fresh', width: 800, height: 600}})
    elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started_at
    message = response.dig('error', 'message').to_s
    File.write(File.join(evidence, 'result.json'), JSON.pretty_generate(response: response, elapsed: elapsed))
    raise "session started with a failing compositor: #{response}" unless response['error']
    raise "exit not reported: #{message}" unless message.include?('kwin_wayland exited with status 3 before creating its wayland-0 socket')
    raise "compositor stderr missing: #{message}" unless message.include?('fake kwin_wayland: refusing to start')
    raise "waited for the socket timeout (#{elapsed.round(1)}s)" if elapsed >= 12
    puts "PASS: reported in #{elapsed.round(1)}s"
  ensure
    input.close
    Timeout.timeout(30) { process.value }
    trace.close
  end
end
