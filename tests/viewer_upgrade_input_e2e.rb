#!/usr/bin/env ruby
# A live viewer without a fresh heartbeat cannot report safe input priority.
require 'json'
require 'open3'
require 'timeout'
require 'tmpdir'
require 'fileutils'

server=File.realpath(ARGV.fetch(0));proof=File.expand_path(ARGV.fetch(1));FileUtils.mkdir_p(proof)
baseline=ENV['KWIN_MCP_E2E_BASELINE']=='1'
class ViewerUpgradeClient
  def initialize(server,home,proof,name,bus,display=nil)
    FileUtils.mkdir_p(%w[.config .cache .local/share .local/state .kde].map { |n|File.join(home,n) })
    env={'HOME'=>home,'XDG_CONFIG_HOME'=>"#{home}/.config",'XDG_CACHE_HOME'=>"#{home}/.cache",'XDG_DATA_HOME'=>"#{home}/.local/share",'XDG_STATE_HOME'=>"#{home}/.local/state",'KDEHOME'=>"#{home}/.kde",'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",'DBUS_SESSION_BUS_ADDRESS'=>bus,'KWIN_MCP_REPO'=>"#{home}/no-source",'CARGO'=>'/usr/bin/false'}
    env['WAYLAND_DISPLAY']=display if display
    @err=File.open(File.join(proof,"#{name}.stderr"),'w');@log=File.open(File.join(proof,"#{name}.jsonl"),'w')
    @input,@output,@process=Open3.popen2(env,server,'--autoclean',err:@err);@id=0
    rpc('initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'viewer-upgrade-input-proof',version:'1'})
    @input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));@input.flush
  end
  def rpc(method,params)
    @id+=1;@input.puts(JSON.generate(jsonrpc:'2.0',id:@id,method:method,params:params));@input.flush
    response=Timeout.timeout(40) do
      loop do
        v=JSON.parse(@output.gets || (raise 'EOF'));break v if v['id']==@id
      end
    end
    @log.puts(JSON.generate(method:method,params:params,response:response));@log.flush;response
  end
  def call(name,args={});rpc('tools/call',name:name,arguments:args);end
  def ok(r)
    raise r.inspect if r['error'] || r.dig('result','isError')
    r.fetch('result').fetch('structuredContent')
  end
  def input(name,args)
    Timeout.timeout(4) do
      loop do
        r=call(name,args)
        return ok(r) unless r.dig('error','data','reason')=='recent_viewer_input'
        sleep 0.1
      end
    end
  end
  def close
    @input.close
    begin
      Timeout.timeout(25) { @process.value }
    rescue Timeout::Error
      Process.kill('TERM',@process.pid);Timeout.timeout(25) { @process.value }
    ensure
      @output.close;@err.close;@log.close
    end
  end
end

Dir.mktmpdir('kmvup-','/var/tmp') do |root|
  daemon=ENV.fetch('PATH').split(':').map { |p|File.join(p,'dbus-daemon') }.find { |p|File.executable?(p) }
  bus=File.join(root,'bus');bus_log=File.open(File.join(proof,'bus.stderr'),'w')
  pid=Process.spawn(daemon,'--session','--nofork','--nopidfile',"--address=unix:path=#{bus}",out:File::NULL,err:bus_log)
  outer=nil;inner=nil;paused=nil
  begin
    Timeout.timeout(5) { sleep 0.02 until File.socket?(bus) }
    outer=ViewerUpgradeClient.new(server,File.join(root,'outer'),proof,'outer',"unix:path=#{bus}")
    desktop=outer.ok(outer.call('session_start',width:1000,height:800,browser_profiles:'fresh'))
    home=File.join(root,'inner')
    inner=ViewerUpgradeClient.new(server,home,proof,'inner',"unix:path=#{bus}",File.join(desktop.fetch('workdir'),'wayland-0'))
    started=inner.ok(inner.call('session_start',width:640,height:480,browser_profiles:'fresh'))
    id=started.fetch('session_id');workdir=started.fetch('workdir')
    inner.ok(inner.call('launch_app',session_id:id,command:'kdialog --title viewer-upgrade-field --inputbox Input > "$HOME/field.txt"'))
    viewer=inner.ok(inner.call('viewer_open',session_id:id,reason:'Inspect the owned input field')).fetch('viewer')
    raise viewer.inspect unless viewer['state']=='ready'
    paused=Integer(viewer.fetch('pid'));Process.kill('STOP',paused)
    activity=File.join(workdir,'viewer-input-activity');File.delete(activity) if File.exist?(activity)
    status_path=File.join(workdir,'viewer-status.json');sleep 3.2
    blocked=inner.call('keyboard_type',session_id:id,text:'not-sent')
    if baseline
      raise 'baseline did not expose unsafe admission' unless blocked.dig('result','structuredContent','submitted_characters')==8
    else
      raise blocked.inspect unless blocked.dig('error','data','reason')=='viewer_activity_check_failed' && blocked.dig('error','data','retry_safe')==true
    end
    reopened=inner.ok(inner.call('viewer_open',session_id:id,reason:'Inspect refreshed viewer')).fetch('viewer')
    raise reopened.inspect unless reopened['state']=='ready' && reopened.fetch('pid')!=paused
    raise 'stale viewer not reaped' if File.exist?("/proc/#{paused}")
    paused=nil
    inner.input('keyboard_type',session_id:id,text:'guard-proof')
    inner.input('keyboard_key',session_id:id,key:'Return')
    result=File.join(proof,'field.txt')
    inner.ok(inner.call('export_file',session_id:id,session_path:File.join(home,'field.txt'),host_path:result))
    expected=baseline ? "not-sentguard-proof\n" : "guard-proof\n"
    raise 'field evidence differs' unless File.read(result)==expected
    inner.ok(inner.call('session_stop',session_id:id))
    outer.ok(outer.call('session_stop',session_id:desktop.fetch('session_id')))
    File.write(File.join(proof,'results.json'),JSON.pretty_generate(baseline:baseline,stale_viewer_input_refused:!baseline,viewer_refreshed:true,exact_field_verified:true))
    puts File.read(File.join(proof,'results.json'))
  ensure
    Process.kill('CONT',paused) if paused && File.exist?("/proc/#{paused}")
    inner&.close;outer&.close
    Process.kill('TERM',pid) rescue Errno::ESRCH
    Process.wait(pid);bus_log.close
  end
end
