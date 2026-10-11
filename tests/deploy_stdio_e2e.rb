#!/usr/bin/env ruby
# Real installed-release stdio verification. No compile occurs on the desktop host.
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'
require 'tmpdir'
require 'base64'

binary = ENV.fetch('KWIN_MCP_DEPLOY_SERVER')
proof = File.realpath(ENV.fetch('KWIN_MCP_DEPLOY_PROOF'))
raise 'Desktop verification must run on engi' unless `hostname -s`.strip == 'engi'
run = Dir.mktmpdir('deploy-stdio-', proof)
home = File.join(run, 'home')
FileUtils.mkdir_p(%w[.config .cache .local/share .local/state].map { |p| File.join(home, p) })
runtime = "/run/user/#{Process.uid}"
environment = {
  'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
  'XDG_CACHE_HOME' => File.join(home, '.cache'),
  'XDG_DATA_HOME' => File.join(home, '.local/share'),
  'XDG_STATE_HOME' => File.join(home, '.local/state'),
  'XDG_RUNTIME_DIR' => runtime,
  'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{runtime}/bus",
  'KWIN_MCP_REPO' => File.join(run, 'no-source-reload')
}
log = File.open(File.join(run, 'rpc.jsonl'), 'w')
stderr = File.open(File.join(run, 'server.log'), 'w')
input, output, process = Open3.popen2(environment, binary, '--autoclean', err: stderr)
serial = 0
session = nil
workdir = nil
request = lambda do |method, parameters|
  serial += 1
  id = serial
  input.puts(JSON.generate(jsonrpc: '2.0', id: id, method: method, params: parameters))
  input.flush
  response = Timeout.timeout(45) do
    loop do
      line = output.gets
      raise 'Server closed stdout' unless line
      parsed = JSON.parse(line)
      # Save images as artifacts and keep protocol logs readable.
      (parsed.dig('result', 'content') || []).each do |content|
        next unless content['type'] == 'image'
        path = File.join(run, "screenshot-#{id}.png")
        File.binwrite(path, Base64.strict_decode64(content.fetch('data')))
        content['data'] = "saved:#{path}"
      end
      log.puts(JSON.generate(parsed))
      log.flush
      break parsed if parsed['id'] == id
    end
  end
  raise response['error'].inspect if response['error']
  result = response.fetch('result')
  raise result.inspect if result['isError']
  result
end
tool = lambda do |name, arguments|
  arguments = arguments.merge('session_id' => session) if session
  request.call('tools/call', 'name' => name, 'arguments' => arguments)
end
begin
  request.call('initialize', 'protocolVersion' => '2024-11-05', 'capabilities' => {},
    'clientInfo' => { 'name' => 'deploy-release-proof', 'version' => '1' })
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
  input.flush
  started = tool.call('session_start', 'width' => 800, 'height' => 600)
  data = started['structuredContent']
  if !data
    text = started.fetch('content').find { |item| item['type'] == 'text' && item['text'].start_with?('{') }
    data = JSON.parse(text.fetch('text')) if text
  end
  session = data && data['session_id']
  if !session
    text = started.fetch('content').map { |item| item['text'] }.compact.join("\n")
    session = text[/session_id: (\S+)/, 1]
  end
  raise 'No session id in session_start response' unless session
  workdir = data && data['workdir']
  tool.call('launch_app', 'command' => 'kate --new')
  windows = tool.call('window_list', {})
  raise 'Kate not present in managed windows' unless JSON.generate(windows).match?(/kate/i)
  tool.call('screenshot', 'inline' => true)
  raise 'No screenshot saved' if Dir.glob(File.join(run, 'screenshot-*.png')).empty?
  tool.call('session_stop', {})
  session = nil
  raise "Session workdir retained: #{workdir}" if workdir && File.exist?(workdir)
  request.call('tools/call', 'name' => 'session_list', 'arguments' => {})
  File.write(File.join(run, 'results.txt'), "initialize=PASS\nstart=PASS\nlaunch=PASS\nwindow_list=PASS\nscreenshot=PASS\nstop=PASS\ncleanup=PASS\n")
ensure
  if session
    begin
      tool.call('session_stop', {})
    rescue StandardError => error
      warn "session_stop retry failed: #{error}"
    end
  end
  input.close
  begin
    Timeout.timeout(15) { process.value }
  rescue Timeout::Error
    Process.kill('TERM', process.pid)
    Timeout.timeout(15) { process.value }
  ensure
    output.close
    stderr.close
    log.close
  end
end
puts "evidence=#{run}"
puts File.read(File.join(run, 'results.txt'))
