#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'

binary, evidence, mode, candidate = ARGV
abort 'usage: session_scope_death_e2e.rb SERVER EVIDENCE kill|oom|upgrade [CANDIDATE]' unless binary && evidence && %w[kill oom upgrade].include?(mode)
binary=File.realpath(binary); evidence=File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
private_binary=File.join(evidence,'kwin-mcp'); FileUtils.cp(binary,private_binary)
home=File.join(evidence,'home')
%w[.config .cache .local/state .local/share .kde].each { |n| FileUtils.mkdir_p(File.join(home,n)) }
runtime="/run/user/#{Process.euid}"; bus="unix:path=#{runtime}/bus"
host_env={'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus}
env=host_env.merge('HOME'=>home,'XDG_CONFIG_HOME'=>home+'/.config','XDG_CACHE_HOME'=>home+'/.cache',
 'XDG_STATE_HOME'=>home+'/.local/state','XDG_DATA_HOME'=>home+'/.local/share','KDEHOME'=>home+'/.kde',
 'KWIN_MCP_REPO'=>evidence+'/no-repo','CARGO'=>'/usr/bin/false')
input=output=error=server=nil; sessions=[]; scopes=[]; workdirs=[]; reloaded=false; next_id=0
trace=File.open(evidence+'/rpc.jsonl','w')
manager=lambda do |*args|
  out,status=Open3.capture2e(host_env,'systemctl','--user',*args)
  raise "systemctl #{args.inspect}: #{out}" unless status.success?
  out.strip
end
rpc=lambda do |method,params|
  next_id+=1
  input.puts(JSON.generate(jsonrpc:'2.0',id:next_id,method:method,params:params)); input.flush
  Timeout.timeout(60) do
    loop do
      line=output.gets or raise 'server EOF'
      reply=JSON.parse(line); trace.puts(JSON.generate(reply)); trace.flush
      reloaded=true if reply['method']=='notifications/tools/list_changed'
      break reply if reply['id']==next_id
    end
  end
end
tool=lambda {|name,args={}| rpc.call('tools/call',{name:name,arguments:args}) }
result=lambda do |reply|
  raise reply.inspect if reply['error'] || reply.dig('result','isError')
  reply.fetch('result').fetch('structuredContent')
end
start=lambda do |args|
  value=result.call(tool.call('session_start',args)); id=value.fetch('session_id'); sessions << id; workdirs << value.fetch('workdir'); value
end
wait_reload=lambda do
  Timeout.timeout(30) { loop do; listed=result.call(tool.call('session_list')); break listed if reloaded; sleep 0.1; end }
end
begin
  input,output,error,server=Open3.popen3(env,private_binary,'--autoclean','--width','640','--height','480','--memory-high','0','--memory-max','2','--memory-swap-max','0')
  diagnostics=Thread.new { File.open(evidence+'/server.stderr','w') {|f| IO.copy_stream(error,f)} }
  initialized=rpc.call('initialize',{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'scope-death-proof',version:'1'}})
  raise initialized.inspect if initialized['error']
  input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized')); input.flush
  if mode=='upgrade'
    raise 'candidate required' unless candidate
    started=start.call({})
    old=result.call(tool.call('session_list')).fetch('sessions').first
    raise 'baseline has memory metadata' if old['memory_limits']
    unit=old.fetch('scope_unit'); scopes << unit
    cgroup='/sys/fs/cgroup'+manager.call('show','-p','ControlGroup','--value',unit)
    manager.call('set-property',unit,'MemoryMax=1610612736','MemorySwapMax=536870912')
    FileUtils.cp(candidate,private_binary+'.next'); File.rename(private_binary+'.next',private_binary)
    listed=wait_reload.call; limits=listed.fetch('sessions').first.fetch('memory_limits')
    raise limits.inspect unless limits['unit']==unit && limits['memory_max']==1_610_612_736 && limits['memory_swap_max']==536_870_912 && limits['memory_high'].nil? && limits['limits_known']==true
    raise 'server PID changed' unless listed['sessions'].first['pid']==server.pid
    result.call(tool.call('session_stop',{session_id:started['session_id']})); sessions.delete(started['session_id'])
    raise 'legacy cgroup survives' if File.exist?(cgroup)
    File.write(evidence+'/verification.json',JSON.pretty_generate(status:'passed',mode:mode,limits:limits,server_pid:server.pid))
  else
    [{memory_max:0},{memory_high:3,memory_max:1},{memory_max:18_446_744_073_709_551_615},{memory_swap_max:18_446_744_073_709_551_615},{memory_high:-1}].each do |args|
      invalid=tool.call('session_start',args); raise invalid.inspect unless invalid.dig('error','code')==-32602
    end
    max=mode=='oom' ? 1 : 2
    started=start.call({memory_high:0,memory_max:max,memory_swap_max:0}); limits=started.fetch('memory_limits'); unit=limits.fetch('unit'); scopes << unit
    raise limits.inspect unless limits['memory_max']==max*1024**3 && limits['memory_high'].nil? && limits['memory_swap_max']==0
    cgroup='/sys/fs/cgroup'+manager.call('show','-p','ControlGroup','--value',unit)
    {'memory.max'=>(max*1024**3).to_s,'memory.high'=>'max','memory.swap.max'=>'0'}.each do |name,expected|
      raise "kernel #{name}" unless File.read(cgroup+'/'+name).strip==expected
    end
    members=Dir.glob(cgroup+'/**/cgroup.procs').flat_map {|f| File.readlines(f).map(&:to_i)}.uniq
    if mode=='oom'
      # Linux cgroup-v2 memory.oom.group applies only to this owned scope.
      File.write(cgroup+'/memory.oom.group', "1\n")
      raise 'group OOM control not applied' unless File.read(cgroup+'/memory.oom.group').strip=='1'
      tool.call('launch_app',{session_id:started['session_id'],command:"ruby -e 'sleep 2; data=[]; 40.times { data << (\"x\" * (64 * 1024 * 1024)); sleep 0.05 }; sleep 30'"})
    else
      manager.call('kill','--signal=KILL',unit)
    end
    diagnostic=nil
    Timeout.timeout(30) do
      loop do
        listed=result.call(tool.call('session_list')); entry=listed['sessions'].find {|s| s['session_id']==started['session_id']}
        if entry['status']=='dead'; diagnostic=entry.fetch('diagnostic'); break; end
        sleep 0.1
      end
    end
    reason=mode=='oom' ? 'oom_kill' : 'sandbox_exited'
    raise diagnostic.inspect unless diagnostic['reason']==reason && diagnostic['memory_max_bytes']==max*1024**3
    if mode=='oom'
      raise diagnostic.inspect unless diagnostic.dig('scope_properties','Result')=='oom-kill'
      raise 'missing memory accounting' unless diagnostic.dig('scope_properties','MemoryPeak').to_i>=1024**3
    end
    failed=tool.call('screenshot',{session_id:started['session_id']})
    raise failed.inspect unless failed.dig('error','data','reason')==reason && failed.dig('error','message').include?('session_start')
    # Keep the dead handle through a real binary reexecution, then route a new
    # live session implicitly without the dead entry causing ambiguity.
    FileUtils.cp(private_binary,private_binary+'.next'); File.rename(private_binary+'.next',private_binary)
    after=wait_reload.call
    raise 'lost death after reload' unless after['sessions'].any? {|s| s['session_id']==started['session_id'] && s['status']=='dead' && s['diagnostic']==diagnostic}
    replacement=start.call({})
    result.call(tool.call('window_list'))
    sessions.dup.each {|id| result.call(tool.call('session_stop',{session_id:id})); sessions.delete(id) }
    Timeout.timeout(10) do
      loop do
        gone=members.all? {|pid| !File.exist?("/proc/#{pid}/stat") || File.read("/proc/#{pid}/stat").split[2]=='Z'}
        break if gone && !File.exist?(cgroup) && workdirs.none? {|p| File.exist?(p)}
        sleep 0.05
      end
    end
    File.write(evidence+'/verification.json',JSON.pretty_generate(status:'passed',mode:mode,limits:limits,diagnostic:diagnostic,members:members,reload_preserved:true))
  end
  puts "scope_#{mode}=PASS"
ensure
  if input && server&.alive?
    sessions.dup.each {|id| tool.call('session_stop',{session_id:id}) rescue nil}
  end
  input&.close rescue nil
  if server
    Timeout.timeout(15) {server.value} rescue (Process.kill('TERM',server.pid) rescue Errno::ESRCH)
  end
  scopes.each {|unit| Open3.capture2e(host_env,'systemctl','--user','stop',unit) }
  diagnostics&.join(5)
  trace.close
end
