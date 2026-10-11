#!/usr/bin/env ruby
# Run on the desktop host with a separately built, private production server:
# ruby tests/transient_focus_e2e.rb /private/evidence/kwin-mcp /private/evidence/results
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'shellwords'

binary, evidence = ARGV
abort 'usage: accessibility_partial_e2e.rb SERVER EVIDENCE_DIR' unless binary && evidence
FileUtils.mkdir_p(evidence)
private_home = File.join(evidence, 'home')
%w[.config .local/share .cache .local/state .kde].each { |dir| FileUtils.mkdir_p(File.join(private_home, dir)) }
env = {'HOME'=>private_home, 'XDG_CONFIG_HOME'=>File.join(private_home,'.config'),
       'XDG_DATA_HOME'=>File.join(private_home,'.local/share'), 'XDG_CACHE_HOME'=>File.join(private_home,'.cache'),
       'XDG_STATE_HOME'=>File.join(private_home,'.local/state'), 'KDEHOME'=>File.join(private_home,'.kde'),
       'KWIN_MCP_REPO'=>File.join(private_home,'no-repository')}
log = File.open(File.join(evidence,'server.log'),'w')
stdin, stdout, stderr, process = Open3.popen3(env, File.expand_path(binary), '--autoclean', '--no-viewer')
stderr_thread = Thread.new { stderr.each_line { |line| log.write(line); log.flush } }
sequence = 0
session = nil
rpc = lambda do |method, params|
  sequence += 1
  request = {jsonrpc:'2.0',id:sequence,method:method,params:params}
  stdin.puts(JSON.generate(request)); stdin.flush
  response = Timeout.timeout(45) do
    loop do
      line = stdout.gets
      raise 'server closed stdout' unless line
      message = JSON.parse(line)
      File.open(File.join(evidence,'protocol.jsonl'),'a') { |f| f.puts(JSON.generate(message)) }
      break message if message['id'] == sequence
    end
  end
  File.write(File.join(evidence,"#{sequence}-response.json"),JSON.pretty_generate(response))
  raise "RPC failed: #{response}" if response['error'] || response.dig('result','isError')
  response.fetch('result')
end
tool = lambda do |name, arguments|
  arguments = arguments.merge('session_id'=>session) if session
  rpc.call('tools/call', {name:name,arguments:arguments})
end
begin
  rpc.call('initialize', {protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'transient-focus-e2e',version:'1'}})
  stdin.puts(JSON.generate({jsonrpc:'2.0',method:'notifications/initialized'})); stdin.flush
  started = tool.call('session_start', {width:1000,height:700})
  session = started.dig('structuredContent','session_id')
  raise 'missing owned session' unless session
  html = 'data:text/html,<title>transient-parent-fixture</title><input type=file autofocus aria-label="Open fixture file">'
  tool.call('launch_app', {command:['google-chrome','--new-window',html].shelljoin})
  windows = tool.call('window_list', {}).dig('structuredContent','windows')
  parent = windows.find { |w| w['title'].include?('transient-parent-fixture') }
  raise "missing Chrome fixture parent: #{windows}" unless parent
  tool.call('keyboard_key', {key:'Return'})
  chooser = nil
  20.times do
    windows = tool.call('window_list', {}).dig('structuredContent','windows')
    chooser = windows.find { |w| w['transientFor'] == parent['id'] && (w['dialog'] || w['modal']) }
    break if chooser
    sleep 0.2
  end
  raise "missing real Chrome chooser: #{windows}" unless chooser
  before = tool.call('screenshot', {}).fetch('structuredContent')
  FileUtils.cp(before.fetch('path'), File.join(evidence,'before.png'))
  tool.call('launch_app', {command:['google-chrome','--new-window','data:text/html,<title>other-window-fixture</title>Other window'].shelljoin})
  activated = tool.call('window_activate', {window_id:parent['id']}).fetch('structuredContent')
  raise "wrong activation target: #{activated}" unless activated.dig('window','id') == chooser['id']
  raise 'request identity lost' unless activated['requested_window_id'] == parent['id']
  windows = tool.call('window_list', {}).dig('structuredContent','windows')
  raise 'compositor focus disagrees' unless windows.any? { |w| w['id']==chooser['id'] && w['active'] }
  screenshot = tool.call('screenshot', {}).fetch('structuredContent')
  raise 'capture/input target disagree' unless screenshot.dig('window','id')==chooser['id']
  FileUtils.cp(screenshot.fetch('path'), File.join(evidence,'focused-chooser.png'))
  File.write(File.join(evidence,'focused-chooser-screenshot.json'),JSON.pretty_generate(screenshot))
  # Escape must reach the chooser and close it, proving the EIS keyboard target.
  tool.call('keyboard_key', {key:'Escape'})
  20.times do
    windows = tool.call('window_list', {}).dig('structuredContent','windows')
    break unless windows.any? { |w| w['id']==chooser['id'] }
    sleep 0.2
  end
  raise 'Escape reached the parent instead of the visible chooser' if windows.any? { |w| w['id']==chooser['id'] }
  after = tool.call('screenshot', {}).fetch('structuredContent')
  FileUtils.cp(after.fetch('path'), File.join(evidence,'after.png'))
  File.write(File.join(evidence,'verification.json'),JSON.pretty_generate({parent:parent['id'],chooser:chooser['id'],activation:activated,status:'passed'}))
ensure
  tool.call('session_stop', {}) if session
  stdin.close
  Timeout.timeout(15) { process.value }
  stderr_thread.join
  log.close
end
