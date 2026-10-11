#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'

binary,evidence,mode=ARGV
abort 'usage: control_channel_failure_e2e.rb SERVER EVIDENCE baseline|candidate' unless binary && evidence && %w[baseline candidate].include?(mode)
evidence=File.expand_path(evidence); home=evidence+'/home'
%w[.config .cache .local/state .local/share .kde].each {|n| FileUtils.mkdir_p(home+'/'+n)}
runtime="/run/user/#{Process.euid}"; bus="unix:path=#{runtime}/bus"
host={'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus}
env=host.merge('HOME'=>home,'XDG_CONFIG_HOME'=>home+'/.config','XDG_CACHE_HOME'=>home+'/.cache','XDG_DATA_HOME'=>home+'/.local/share','XDG_STATE_HOME'=>home+'/.local/state','KDEHOME'=>home+'/.kde','KWIN_MCP_REPO'=>evidence+'/no-repository','CARGO'=>'/usr/bin/false')
i,o,e,server=Open3.popen3(env,File.realpath(binary),'--autoclean','--width','800','--height','600')
diagnostics=Thread.new {File.open(evidence+'/server.stderr','w') {|f|IO.copy_stream(e,f)}}
trace=File.open(evidence+'/rpc.jsonl','w'); number=0; ids=[]; workdirs=[]; units=[]
rpc=lambda do |method,params|
  number+=1;i.puts(JSON.generate(jsonrpc:'2.0',id:number,method:method,params:params));i.flush
  Timeout.timeout(60) {loop do; line=o.gets or raise 'EOF'; reply=JSON.parse(line);trace.puts(JSON.generate(reply));trace.flush; break reply if reply['id']==number;end}
end
tool=lambda {|name,args={}| rpc.call('tools/call',{name:name,arguments:args})}
result=lambda do |reply|
  raise reply.inspect if reply['error'] || reply.dig('result','isError')
  reply.fetch('result').fetch('structuredContent')
end
start=lambda do
  value=result.call(tool.call('session_start'));ids << value.fetch('session_id');workdirs << value.fetch('workdir');units << value.dig('memory_limits','unit');value
end
capture=lambda do |id,name|
  result.call(tool.call('launch_app',{session_id:id,command:"kdialog --title kwin252 --msgbox 'Private control channel verification'"}))
  frame=result.call(tool.call('screenshot',{session_id:id}))
  path=frame.fetch('path');FileUtils.cp(path,evidence+'/'+name+'.png');frame
end
begin
  initialized=rpc.call('initialize',{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'control-channel-proof',version:'1'}})
  raise initialized.inspect if initialized['error']
  i.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));i.flush
  started=start.call; id=started['session_id']; before=capture.call(id,'before')
  listing=result.call(tool.call('session_list')).fetch('sessions').find {|s|s['session_id']==id}
  sandbox=listing.fetch('sandbox_pid'); unit=listing.fetch('scope_unit')
  group,status=Open3.capture2(host,'systemctl','--user','show','-p','ControlGroup','--value',unit)
  raise 'missing private cgroup' unless status.success? && group.strip.end_with?(unit)
  cgroup='/sys/fs/cgroup'+group.strip
  members=Dir.glob(cgroup+'/**/cgroup.procs').flat_map {|f|File.readlines(f).map(&:to_i)}.uniq
  # Identify the exact private session bus by its actual address, not merely
  # by the common dbus-daemon name; host and accessibility buses are excluded.
  address='--address=unix:path='+started.fetch('workdir')+'/bus'
  daemons=members.select do |pid|
    File.read("/proc/#{pid}/comm").strip=='dbus-daemon' && File.binread("/proc/#{pid}/cmdline").split("\0").include?(address)
  rescue Errno::ENOENT
    false
  end
  raise "expected exactly one private bus: #{daemons}" unless daemons.length==1
  target=daemons.first; Process.kill('KILL',target)
  failed=nil
  Timeout.timeout(10) do
    loop do
      failed=tool.call('screenshot',{session_id:id}); break if failed['error'];sleep 0.05
    end
  end
  message=failed.dig('error','message'); data=failed.dig('error','data')
  raise failed.inspect unless message.match?(/Broken pipe|Connection reset|connection closed/i)
  raise 'launcher also died; did not exercise the live-launcher branch' unless File.exist?("/proc/#{sandbox}/stat") && File.read("/proc/#{sandbox}/stat").split[2]!='Z'
  if mode=='candidate'
    raise failed.inspect unless data && data['reason']=='session_channel_failed' && data['channel']=='kwin_control' && data['operation']=='screenshot' && data['session_id']==id && data['sandbox_exit_confirmed']==false && data['oom_confirmed']==false
    raise message unless message.include?('KWin') && message.include?('session_stop') && message.include?('session_start')
  else
    raise 'baseline already annotated' if data && data['reason']=='session_channel_failed'
  end
  result.call(tool.call('session_stop',{session_id:id}));ids.delete(id)
  raise 'failed scope/workdir survived stop' if File.exist?(cgroup) || File.exist?(started['workdir'])
  replacement=start.call; after=capture.call(replacement['session_id'],'replacement')
  result.call(tool.call('session_stop',{session_id:replacement['session_id']}));ids.delete(replacement['session_id'])
  raise 'replacement workdir survived' if File.exist?(replacement['workdir'])
  File.write(evidence+'/verification.json',JSON.pretty_generate(status:'passed',mode:mode,failed:failed,private_bus_pid:target,sandbox_pid:sandbox,before:before,replacement:after,scope_removed:cgroup))
  puts "control_channel_#{mode}=PASS"
ensure
  ids.each {|id| tool.call('session_stop',{session_id:id}) rescue nil}
  i.close rescue nil
  Timeout.timeout(15) {server.value} rescue (Process.kill('TERM',server.pid) rescue Errno::ESRCH)
  units.compact.each {|u|Open3.capture2e(host,'systemctl','--user','stop',u)}
  diagnostics.join(5);trace.close
end
