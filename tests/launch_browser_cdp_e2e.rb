#!/usr/bin/env ruby
# Exercise the production stdio path with installed Google Chrome and a private
# session profile. The explicit invalid port proves launch_app uses its session
# port for both the browser and its CDP connection.
require 'fileutils'
require 'json'
require 'open3'
require 'net/http'
require 'timeout'

server, artifacts = ARGV
abort 'usage: launch_browser_cdp_e2e.rb /path/to/kwin-mcp /path/to/artifacts' unless server && artifacts
server = File.realpath(server)
artifacts = File.expand_path(artifacts)
abort 'google-chrome-stable is not on PATH' unless ENV.fetch('PATH', '').split(File::PATH_SEPARATOR).any? do |directory|
  File.executable?(File.join(directory, 'google-chrome-stable'))
end
FileUtils.mkdir_p(artifacts, mode: 0o700)
private_home = File.join(artifacts, 'home')
%w[.config .cache .local/share .local/state .kde].each do |name|
  FileUtils.mkdir_p(File.join(private_home, name), mode: 0o700)
end
runtime_dir = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.euid}")
environment = {
  'HOME' => private_home,
  'XDG_CONFIG_HOME' => File.join(private_home, '.config'),
  'XDG_CACHE_HOME' => File.join(private_home, '.cache'),
  'XDG_DATA_HOME' => File.join(private_home, '.local/share'),
  'XDG_STATE_HOME' => File.join(private_home, '.local/state'),
  'KDEHOME' => File.join(private_home, '.kde'),
  'KWIN_MCP_REPO' => File.join(artifacts, 'no-source'),
  'CARGO' => '/usr/bin/false',
  'XDG_RUNTIME_DIR' => runtime_dir,
  'DBUS_SESSION_BUS_ADDRESS' => ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime_dir}/bus")
}

input, output, errors, process = Open3.popen3(environment, server, '--autoclean', '--no-viewer')
stderr = Thread.new do
  File.open(File.join(artifacts, 'server.stderr.log'), 'w') do |log|
    errors.each_line { |line| log.write(line); log.flush }
  end
end
serial = 0
session_id = nil
rpc = lambda do |method, params|
  serial += 1
  input.puts(JSON.generate(jsonrpc: '2.0', id: serial, method: method, params: params))
  input.flush
  Timeout.timeout(240) do
    loop do
      response = JSON.parse(output.gets || raise('MCP server closed stdout'))
      break response if response['id'] == serial
    end
  end
end
call = lambda do |name, arguments = {}|
  arguments = arguments.merge(session_id: session_id) if session_id
  response = rpc.call('tools/call', {name: name, arguments: arguments})
  raise "MCP failure: #{response}" if response['error'] || response.dig('result', 'isError')
  response.fetch('result').fetch('structuredContent')
end

begin
  rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'launch-browser-cdp-e2e', version: '1'}})
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
  input.flush
  session_id = call.call('session_start', {
    width: 1000,
    height: 700,
    browser_profiles: 'fresh'
  }).fetch('session_id')
  launched = call.call('launch_app', {
    command: 'google-chrome-stable --no-first-run --no-default-browser-check --user-data-dir="$HOME/.kwin-mcp-cdp-e2e" --remote-debugging-address=127.0.0.1 --remote-debugging-port=1 about:blank'
  })
  raise "CDP did not connect: #{launched}" unless launched['cdp'] == true
  port = launched.fetch('cdp_port')
  raise "session port was not assigned: #{port}" unless port.is_a?(Integer) && port > 1024 && port != 1
  http = Net::HTTP.new('127.0.0.1', port, nil)
  http.open_timeout = 10
  http.read_timeout = 10
  response = http.get('/json/version')
  raise "CDP host port returned an error: #{response.code}" unless response.code == '200'
  metadata = JSON.parse(response.body)
  raise 'CDP endpoint omitted browser metadata' unless metadata['Browser'] && metadata['webSocketDebuggerUrl']
  puts "PASS: Chrome CDP connected through session port #{port}, replacing requested port 1"
ensure
  begin
    call.call('session_stop') if session_id
  rescue StandardError => error
    warn "session cleanup: #{error}"
  ensure
    input.close unless input.closed?
    begin
      Timeout.timeout(45) { process.value }
    rescue Timeout::Error
      Process.kill('TERM', process.pid) if process.alive?
      begin
        Timeout.timeout(45) { process.value }
      rescue Timeout::Error
        Process.kill('KILL', process.pid) if process.alive?
        process.value
        raise 'test server did not finish session cleanup'
      end
    end
    stderr.join
    errors.close unless errors.closed?
    output.close unless output.closed?
  end
end
