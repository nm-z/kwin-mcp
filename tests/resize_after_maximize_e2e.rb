#!/usr/bin/env ruby
# Run on the desktop host with a separately built, private production server:
# ruby tests/resize_after_maximize_e2e.rb /private/evidence/kwin-mcp /private/evidence/results
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
  rpc.call('initialize', {protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'resize-frame-e2e',version:'1'}})
  stdin.puts(JSON.generate({jsonrpc:'2.0',method:'notifications/initialized'})); stdin.flush
  started = tool.call('session_start', {width:1000,height:700})
  session = started.dig('structuredContent','session_id')
  raise 'missing owned session' unless session
  tool.call('launch_app', {command:'kwrite'})
  window = tool.call('window_list', {}).dig('structuredContent','windows').find { |w| w['active'] }
  raise "window did not start maximized: #{window}" unless window && window['frameGeometry']['width']==1000 && window['frameGeometry']['height']==700
  before = tool.call('screenshot', {}).fetch('structuredContent')
  FileUtils.cp(before.fetch('path'), File.join(evidence,'before.png'))
  raise 'initial PNG dimensions disagree' unless File.binread(File.join(evidence,'before.png'))[16,8].unpack('NN') == [1000,700]
  tool.call('keyboard_key', {key:'super+Backspace'})
  sleep 0.5
  restored = tool.call('window_list', {}).dig('structuredContent','windows').find { |w| w['id']==window['id'] }
  raise 'window identity changed' unless restored && restored['pid']==window['pid']
  # Native KWin resize works after restoring: measure the real restored corner.
  from_x = restored['w'].to_i-10
  from_y = restored['h'].to_i-10
  tool.call('keyboard_press', {key:'super'})
  begin
    tool.call('mouse_drag', {from_x:from_x,from_y:from_y,to_x:from_x,to_y:from_y-100,button:'right'})
  ensure
    tool.call('keyboard_release', {key:'super'})
  end
  resized = nil
  20.times do
    resized = tool.call('window_list', {}).dig('structuredContent','windows').find { |w| w['id']==window['id'] }
    break if resized && resized['frameGeometry']['height'] < restored['frameGeometry']['height']
    sleep 0.2
  end
  raise "actual frame did not shrink: #{resized}" unless resized && resized['frameGeometry']['height'] < restored['frameGeometry']['height']
  raise 'window identity changed on resize' unless resized['pid']==window['pid']
  expected_height = restored['frameGeometry']['height']-100
  raise "unexpected resize geometry: #{resized}" unless (resized['frameGeometry']['height']-expected_height).abs <= 2 && resized['frameGeometry']['width']==restored['frameGeometry']['width']
  after = tool.call('screenshot', {}).fetch('structuredContent')
  FileUtils.cp(after.fetch('path'), File.join(evidence,'after.png'))
  png_size = File.binread(File.join(evidence,'after.png'))[16,8].unpack('NN')
  actual_size = [resized['frameGeometry']['width'].to_i,resized['frameGeometry']['height'].to_i]
  raise "PNG/frame disagree: #{png_size} vs #{actual_size}" unless png_size==actual_size
  raise 'captured a different window' unless after.dig('window','id')==window['id']
  File.write(File.join(evidence,'verification.json'),JSON.pretty_generate({initial:window,restored:restored,resized:resized,png_dimensions:png_size,status:'passed'}))
ensure
  tool.call('session_stop', {}) if session
  stdin.close
  Timeout.timeout(15) { process.value }
  stderr_thread.join
  log.close
end
