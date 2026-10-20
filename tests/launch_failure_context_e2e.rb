#!/usr/bin/env ruby
# Real OS write failure in an owned desktop; no host quota or live session is changed.
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'shellwords'
binary,evidence,mode=ARGV
abort 'usage: launch_failure_context_e2e.rb SERVER EVIDENCE baseline|candidate' unless binary && evidence && %w[baseline candidate].include?(mode)
evidence=File.expand_path(evidence);home=File.join(evidence,'home')
%w[.config .cache .local/state .local/share .kde].each {|d|FileUtils.mkdir_p(File.join(home,d))}
runtime="/run/user/#{Process.uid}"
env={'HOME'=>home,'XDG_CONFIG_HOME'=>home+'/.config','XDG_CACHE_HOME'=>home+'/.cache','XDG_DATA_HOME'=>home+'/.local/share','XDG_STATE_HOME'=>home+'/.local/state','KDEHOME'=>home+'/.kde','XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{runtime}/bus",'KWIN_MCP_REPO'=>home+'/no-source','CARGO'=>'/usr/bin/false','DISPLAY'=>nil,'WAYLAND_DISPLAY'=>nil,'WAYLAND_SOCKET'=>nil}
i,o,e,process=Open3.popen3(env,File.realpath(binary),'--autoclean','--no-viewer')
reader=Thread.new {File.open(evidence+'/server.stderr','w') {|f|IO.copy_stream(e,f)}}
trace=File.open(evidence+'/rpc.jsonl','w');sequence=0;session=nil;work=nil;permissions=nil
rpc=lambda do |method,params|
 sequence+=1;i.puts(JSON.generate(jsonrpc:'2.0',id:sequence,method:method,params:params));i.flush
 reply=Timeout.timeout(70) {loop {line=o.gets or raise 'server EOF';item=JSON.parse(line);break item if item['id']==sequence}}
 trace.puts(JSON.generate(method:method,params:params,response:reply));trace.flush;reply
end
call=lambda do |name,args={},allow_error=false|
 args=args.merge(session_id:session) if session && name!='session_start'
 reply=rpc.call('tools/call',{name:name,arguments:args})
 raise reply.inspect if !allow_error && (reply['error'] || reply.dig('result','isError'))
 allow_error ? reply : reply.fetch('result').fetch('structuredContent')
end
begin
 rpc.call('initialize',{protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'issue500-failure-context',version:'1'}})
 i.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));i.flush
 started=call.call('session_start',{width:800,height:600,browser_profiles:'fresh'})
 session=started.fetch('session_id');work=started.fetch('workdir')
 raise 'workdir is not owned by test user' unless File.stat(work).uid==Process.uid
 permissions=File.stat(work).mode & 0777
 # Exercise the production script write with a real EACCES. The earlier EDQUOT
 # report's exact errno is covered by unit diagnostics, not by filling a host disk.
 File.chmod(0500,work)
 marker=File.join(work,'issue500-command-ran')
 failed=call.call('launch_app',{command:"printf 'should-not-run' > #{marker.shellescape}",window_timeout_seconds:1},true)
 File.chmod(permissions,work)
 raise "expected a write failure: #{failed}" unless failed['error']
 raise 'command was executed despite the failed preflight write' if File.exist?(marker)
 data=failed.dig('error','data') || {}
 raise failed.inspect unless data['command_submitted']==false && data['path']&.start_with?(work+'/launch-')
 if mode=='candidate'
  raise failed.inspect unless data['operation']=='launch_app' && data['retry_safe']==true && data['os_error']==13 && data['stage']=='writing the launch script'
  raise failed.inspect unless data.fetch('recovery_action').include?('quota') && failed.dig('error','message').include?('Permission denied')
 else
  raise 'baseline already has uniform launch recovery' if data['operation']=='launch_app' && data['retry_safe']==true
 end
 title="issue500-recovered-#{Process.pid}"
 call.call('launch_app',{command:['kdialog','--title',title,'--msgbox','Session usable after corrected launch error'].shelljoin})
 windows=call.call('window_list').fetch('windows')
 raise 'same session did not recover' unless windows.any? {|w| w['title'].include?(title)}
 frame=call.call('screenshot',{inline:false})
 FileUtils.cp(frame.fetch('path'),evidence+'/recovered.png')
 proof={status:'passed',mode:mode,failed:failed,command_marker_absent:!File.exist?(marker),same_session_recovered:true,errno_exercised:'EACCES',quota_not_changed:true}
 File.write(evidence+'/verification.json',JSON.pretty_generate(proof));puts JSON.generate(proof)
ensure
 File.chmod(permissions,work) if work && permissions && File.directory?(work)
 call.call('session_stop') if session
 i.close
 Timeout.timeout(20) {process.value}
 reader.join;trace.close
end
