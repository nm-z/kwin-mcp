#!/usr/bin/env ruby
# Run on the desktop host with a separately built, private production server:
# ruby tests/accessibility_partial_e2e.rb /private/evidence/kwin-mcp /private/evidence/results
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
provider_pid = nil
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
  rpc.call('initialize', {protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'partial-tree-e2e',version:'1'}})
  stdin.puts(JSON.generate({jsonrpc:'2.0',method:'notifications/initialized'})); stdin.flush
  started = tool.call('session_start', {width:800,height:600})
  session = started.dig('structuredContent','session_id')
  raise 'missing owned session' unless session
  title = "partial-tree-fixture-#{Process.pid}"
  command = ['kdialog','--title',title,'--checklist','Bounded partial accessibility tree']
  2500.times { |i| command.concat(["tag#{i}","Choice #{i}",'off']) }
  tool.call('launch_app', {command:command.shelljoin})
  # Pause only this real fixture's AT-SPI provider. Matching both the unique
  # title argument and executable excludes shells and other desktop apps.
  providers = Dir.glob('/proc/[0-9]*').filter_map do |proc_dir|
    pid = File.basename(proc_dir).to_i
    begin
      next unless File.stat(proc_dir).uid == Process.uid
      next unless File.basename(File.readlink(File.join(proc_dir,'exe'))) == 'kdialog'
      pid if File.binread(File.join(proc_dir,'cmdline')).split("\0").include?(title)
    rescue Errno::ENOENT, Errno::EACCES
      nil
    end
  end
  raise "expected exactly one owned kdialog provider: #{providers}" unless providers.length==1
  provider_pid = providers.first
  tool.call('accessibility_tree', {max_depth:0,show_elements:true,app_name:'kdialog'})
  before = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  stopper = Thread.new { sleep 0.3; Process.kill('STOP',provider_pid) }
  begin
    default_result = tool.call('accessibility_tree', {max_depth:12,show_elements:true,app_name:'kdialog'})
  ensure
    stopper.join
    Process.kill('CONT',provider_pid)
  end
  default_elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-before
  default_info = default_result.fetch('structuredContent')
  raise "expected default traversal timeout, got #{default_info}" unless default_info['incomplete']
  raise "wrong default timeout reason: #{default_info}" unless default_info['reason']=='traversal reached the 5 second limit'
  raise "wrong default timeout metadata: #{default_info}" unless default_info['timeout_seconds']==5
  raise "default traversal ended outside its budget: #{default_elapsed}" unless default_elapsed >= 4.5 && default_elapsed < 8

  before = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  stopper = Thread.new { sleep 0.3; Process.kill('STOP',provider_pid) }
  begin
    result = tool.call('accessibility_tree', {max_depth:12,show_elements:true,app_name:'kdialog',timeout_seconds:6})
  ensure
    stopper.join
    Process.kill('CONT',provider_pid)
  end
  elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-before
  info = result.fetch('structuredContent')
  text = result.fetch('content').select { |item| item['type']=='text' }.map { |item| item['text'] }.join("\n")
  raise "expected bounded partial tree, got #{info}" unless info['incomplete']
  raise "tree exceeded character budget: #{info}" unless info['chars'] <= 30_000
  raise "wrong timeout reason: #{info}" unless info['reason']=='traversal reached the 6 second limit'
  raise "wrong timeout metadata: #{info}" unless info['timeout_seconds']==6
  raise "tree exceeded or missed its requested time budget: #{elapsed}" unless elapsed >= 5.5 && elapsed < 10
  root = text[/root=([^\s]+)/,1]
  if info['nodes'].positive?
    raise 'missing actionable root' unless root
    resumed = tool.call('accessibility_tree', {root:root,max_depth:0,show_elements:true})
    raise "root continuation failed: #{resumed}" unless resumed.dig('structuredContent','nodes').positive?
  else
    raise 'missing actionable empty-tree fallback' unless info.fetch('continuation').include?('screenshot')
  end
  tool.call('screenshot', {})
  File.write(File.join(evidence,'verification.json'), JSON.pretty_generate({elapsed:elapsed,tree:info,continuation:root,status:'passed'}))
ensure
  begin
    Process.kill('CONT',provider_pid) if provider_pid
  rescue Errno::ESRCH
    # The fixture already exited.
  end
  tool.call('session_stop', {}) if session
  stdin.close
  Timeout.timeout(15) { process.value }
  stderr_thread.join
  log.close
end
