#!/usr/bin/env ruby
# Real native and Chrome controls, a real DOM overlay, and same-user forwarding.
require 'json'
require 'open3'
require 'fileutils'
require 'shellwords'
require 'timeout'
binary,evidence,mode=ARGV
abort 'usage: ui_match_context_e2e.rb SERVER EVIDENCE baseline|candidate' unless binary && evidence && %w[baseline candidate].include?(mode)
evidence=File.expand_path(evidence);home=evidence+'/home'
%w[.config .cache .local/share .local/state .kde].each {|d|FileUtils.mkdir_p(home+'/'+d)}
runtime="/run/user/#{Process.uid}"
env={'HOME'=>home,'XDG_CONFIG_HOME'=>home+'/.config','XDG_CACHE_HOME'=>home+'/.cache','XDG_DATA_HOME'=>home+'/.local/share','XDG_STATE_HOME'=>home+'/.local/state','KDEHOME'=>home+'/.kde','XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{runtime}/bus",'KWIN_MCP_REPO'=>home+'/no-source','CARGO'=>'/usr/bin/false','DISPLAY'=>nil,'WAYLAND_DISPLAY'=>nil,'WAYLAND_SOCKET'=>nil}
clients=[];session=nil
client=lambda do |name|
 input,output,error,process=Open3.popen3(env,File.realpath(binary),'--autoclean','--no-viewer')
 reader=Thread.new {File.open(evidence+'/'+name+'.stderr','w') {|f|IO.copy_stream(error,f)}}
 trace=File.open(evidence+'/'+name+'-rpc.jsonl','w');sequence=0
 rpc=lambda do |method,params|
  sequence+=1;input.puts(JSON.generate(jsonrpc:'2.0',id:sequence,method:method,params:params));input.flush
  started=Process.clock_gettime(Process::CLOCK_MONOTONIC)
  reply=Timeout.timeout(80) {loop {line=output.gets or raise 'server EOF';v=JSON.parse(line);break v if v['id']==sequence}}
  trace.puts(JSON.generate(method:method,params:params,response:reply,elapsed:Process.clock_gettime(Process::CLOCK_MONOTONIC)-started));trace.flush
  raise reply.inspect if reply['error'] || reply.dig('result','isError')
  reply.fetch('result')
 end
 clients << [input,process,reader,trace]
 rpc.call('initialize',{protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'ui-match-context',version:'1'}})
 input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));input.flush
 [rpc,process.pid]
end
owner,owner_pid=client.call('owner')
tool=lambda do |rpc,name,args={}|
 args=args.merge(session_id:session) if session && name!='session_start'
 rpc.call('tools/call',{name:name,arguments:args}).fetch('structuredContent')
end
capture=lambda do |name|
 image=tool.call(owner,'screenshot',{inline:false});FileUtils.cp(image.fetch('path'),evidence+'/'+name+'.png')
end
begin
 started=tool.call(owner,'session_start',{width:1000,height:700,browser_profiles:'fresh'})
 session=started.fetch('session_id');work=started.fetch('workdir')
 title="UI418-native-#{Process.pid}"
 tool.call(owner,'launch_app',{command:['kdialog','--title',title,'--yesno','Stop is explanatory text, not the button.','--yes-label','Stop','--no-label','Cancel'].shelljoin})
 native=tool.call(owner,'find_ui_elements',{query:'Stop',role:'button'})
 capture.call('native')
 native_click_verified=false
 if mode=='candidate'
  native_matches=native.fetch('elements').select {|el|el['backend']=='atspi'}
  raise "no native button: #{native}" unless native_matches.any? {|el|el['name']=='Stop'}
  raise "role filter leaked static text: #{native_matches}" unless native_matches.all? {|el|['button','push button','toggle button'].include?(el['role'])}
  stop=native_matches.find {|el|el['name']=='Stop'}
  raise stop.inspect unless stop.dig('surface','application') && stop.dig('surface','frame_title') && stop['coordinate_space'] && stop['window_association']
  if stop['mouse_coordinates_usable']
   box=stop.fetch('bounds')
   tool.call(owner,'mouse_click',{x:(box['x']+box['w']/2.0).round,y:(box['y']+box['h']/2.0).round})
   sleep 0.3
   native_click_verified=tool.call(owner,'window_list').fetch('windows').none? {|w|w['title'].include?(title)}
   raise 'advertised native coordinates did not close the expected dialog' unless native_click_verified
  else
   tool.call(owner,'keyboard_key',{key:'ESC'})
  end
 else
  raise 'baseline already returns the new match context' if native.key?('elements')
  tool.call(owner,'keyboard_key',{key:'ESC'})
 end
 html=File.join(work,'controls.html')
 File.write(html,<<~HTML)
 <!doctype html><title>UI418 controls</title>
 <style>body{margin:0;background:white;font:22px sans-serif}button{position:absolute;left:80px;width:250px;height:60px;font-size:20px}#free{top:60px}#covered{top:170px}#cover{position:absolute;left:70px;top:160px;width:280px;height:90px;background:#555;color:white;z-index:9}#explanation{position:absolute;top:290px}</style>
 <button id="free" onclick="document.title='UI418 clicked'">Stop ui418-free</button><button id="covered">Stop ui418-covered</button>
 <div id="cover">Overlay covers the second button</div><p role="text" id="explanation">Stop is only explanatory text here</p>
 HTML
 launched=tool.call(owner,'launch_app',{command:['google-chrome-stable','--no-first-run','--no-default-browser-check','--disable-sync',"--user-data-dir=#{work}/chrome-profile","file://#{html}"].shelljoin})
 raise "CDP was not established: #{launched}" unless launched['cdp_port']
 tool.call(owner,'mouse_click',{x:700,y:500})
 direct=tool.call(owner,'find_ui_elements',{query:'Stop',role:'button'})
 capture.call('browser')
 forwarded_rpc,forward_pid=client.call('forwarder')
 forwarded=tool.call(forwarded_rpc,'find_ui_elements',{query:'Stop',role:'button'})
 if mode=='candidate'
  [direct,forwarded].each do |reply|
   dom=reply.fetch('elements').select {|el|el['backend']=='cdp'}
   free=dom.find {|el|el['name']=='Stop ui418-free'} or raise "free missing: #{reply}"
   covered=dom.find {|el|el['name']=='Stop ui418-covered'} or raise "covered missing: #{reply}"
   raise "role leaked: #{dom}" unless dom.all? {|el|el['role']=='button'}
   raise "free hit test: #{free}" unless free['center_hit']==true && free['occluded']==false
   raise "covered hit test: #{covered}" unless covered['center_hit']==false && covered['occluded']==true && covered['clickable']==false
   raise free.inspect unless free['coordinate_space']=='css_viewport_pixels' && free['mouse_coordinates_usable']==false && free.dig('surface','url').end_with?('/controls.html') && free.dig('surface','viewport','device_pixel_ratio')
   raise 'owner identity lost in forwarding' unless reply['server_pid']==owner_pid && reply['server_pid']!=forward_pid
   raise 'active window provenance absent' unless reply['active_window_id'] && reply['sampled_at_milliseconds']
  end
 else
  raise 'baseline already returns context' if direct.key?('elements') || forwarded.key?('elements')
 end
 browser_click_verified=false
 if mode=='candidate'
  free=forwarded.fetch('elements').find {|el|el['backend']=='atspi' && el['name']=='Stop ui418-free'}
  raise "browser window coordinates were not established: #{free}" unless free && free['mouse_coordinates_usable']
  box=free.fetch('bounds');tool.call(forwarded_rpc,'mouse_click',{x:(box['x']+box['w']/2.0).round,y:(box['y']+box['h']/2.0).round})
  sleep 0.3
  browser_click_verified=tool.call(owner,'window_list').fetch('windows').any? {|w|w['title'].include?('UI418 clicked')}
  raise 'forwarded click missed the real browser control' unless browser_click_verified
  capture.call('clicked-browser')
 end
 zoom_proof=nil
 if ENV['KWIN_PROOF_ZOOM']=='1'
  zoom_html=File.join(work,'zoom-controls.html')
  token='zoom407-proof'
  File.write(zoom_html,<<~HTML)
  <!doctype html><title>ZOOM407 controls</title>
  <style>body{margin:0;background:white;font:16px sans-serif}label{position:absolute;left:20px}input,textarea{position:absolute;box-sizing:border-box;font:16px sans-serif}#milestone{left:60px;top:50px;width:300px;height:45px}#comment{left:100px;top:180px;width:300px;height:65px}</style>
  <label style="top:20px">Set milestone (not the intended field)</label><input id="milestone" aria-label="Set milestone">
  <label style="top:145px">Add a comment</label><textarea id="comment" aria-label="Add a comment">Add a comment</textarea>
  <script>
  const comment=document.getElementById('comment'), milestone=document.getElementById('milestone');
  function report(){document.title=comment.value==='#{token}' && milestone.value==='' ? 'COMMENT_OK407' : milestone.value==='#{token}' ? 'MILESTONE_WRONG407' : 'ZOOM407 input';}
  comment.addEventListener('input',report);milestone.addEventListener('input',report);
  </script>
  HTML
  tool.call(owner,'keyboard_key',{key:'CTRL+L'})
  tool.call(owner,'keyboard_type',{text:"file://#{zoom_html}",speed:8.0})
  tool.call(owner,'keyboard_key',{key:'ENTER'})
  sleep 0.5
  tool.call(owner,'keyboard_key',{key:'CTRL+0'})
  4.times {tool.call(owner,'keyboard_key',{key:'CTRL+EQUAL'})}
  sleep 1
  zoom=tool.call(forwarded_rpc,'find_ui_elements',{query:'Add a comment'})
  capture.call('zoom175-before')
  if mode=='candidate'
   dom=zoom.fetch('elements').find {|el|el['backend']=='cdp' && el['role']=='textbox' && el['name']=='Add a comment'}
   raise "missing scaled DOM target: #{zoom}" unless dom
   raise "expected actual browser 175% zoom: #{dom}" unless (dom.dig('surface','viewport','device_pixel_ratio').to_f-1.75).abs<0.01
   target=zoom.fetch('elements').find {|el|el['backend']=='atspi' && el['name']=='Add a comment' && ['entry','text','textbox'].include?(el['role'])}
   raise "native scaled target unverified: #{zoom}" unless target && target['mouse_coordinates_usable']
   box=target.fetch('bounds')
  else
   row=zoom.fetch('results').lines.find {|line|line.start_with?("textarea\t")}
   raise "baseline textarea absent: #{zoom}" unless row
   values=row.match(/\((-?\d+), (-?\d+), (\d+)x(\d+)\)/).captures.map(&:to_i)
   box={'x'=>values[0],'y'=>values[1],'w'=>values[2],'h'=>values[3]}
  end
  tool.call(forwarded_rpc,'mouse_click',{x:(box['x']+box['w']/2.0).round,y:(box['y']+box['h']/2.0).round})
  tool.call(owner,'keyboard_key',{key:'CTRL+A'})
  typed=tool.call(owner,'keyboard_type',{text:token,speed:8.0})
  # KWin publishes the changed browser caption asynchronously after the input event.
  # Wait read-only for that independent observation; never replay any input.
  expected=mode=='candidate' ? 'COMMENT_OK407' : 'MILESTONE_WRONG407'
  deadline=Process.clock_gettime(Process::CLOCK_MONOTONIC)+3
  title407=nil
  loop do
   title407=tool.call(owner,'window_list').fetch('windows').find {|w|w['active']}.fetch('title')
   break if title407.include?(expected) || Process.clock_gettime(Process::CLOCK_MONOTONIC)>=deadline
   sleep 0.05
  end
  capture.call('zoom175-after')
  raise "wrong independent target outcome: #{title407}, #{typed}" unless title407.include?(expected)
  zoom_proof={query:zoom,typed:typed,actual_title:title407,expected_title:expected,independent_field_verified:true}
 end
 proof={status:'passed',mode:mode,native:native,direct:direct,forwarded:forwarded,owner_pid:owner_pid,forwarder_pid:forward_pid,native_click_verified:native_click_verified,browser_click_verified:browser_click_verified,zoom175:zoom_proof}
 File.write(evidence+'/verification.json',JSON.pretty_generate(proof));puts JSON.generate({status:'passed',mode:mode,native_matches:native['matches'],browser_matches:direct['matches'],native_click_verified:native_click_verified,browser_click_verified:browser_click_verified})
ensure
 tool.call(owner,'session_stop') if session
 clients.reverse_each do |input,process,reader,trace|
  input.close
  Timeout.timeout(20) {process.value}
  reader.join;trace.close
 end
end
