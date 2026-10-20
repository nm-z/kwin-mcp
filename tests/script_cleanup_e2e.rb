#!/usr/bin/env ruby
# Pause only this test's real compositor after its result and before cleanup.
require 'json'
require 'open3'
require 'fileutils'
require 'tmpdir'
require_relative 'fixture_paths'
require 'timeout'

server=File.realpath(ARGV.fetch(0))
evidence=File.expand_path(ARGV.fetch(1))
FileUtils.mkdir_p(evidence)

def compositor_under(root,workdir)
  pending=[root]; seen={}
  until pending.empty?
    pid=pending.shift
    next if seen[pid]
    seen[pid]=true
    begin
      if File.read("/proc/#{pid}/comm").strip=='kwin_wayland' && File.binread("/proc/#{pid}/environ").split("\0").include?("XDG_RUNTIME_DIR=#{workdir}")
        return pid
      end
      pending.concat(File.read("/proc/#{pid}/task/#{pid}/children").split.map(&:to_i))
    rescue Errno::ENOENT,Errno::ESRCH
    end
  end
  raise 'owned compositor not found under sandbox launcher'
end

Dir.mktmpdir('kmscript-',FixturePaths.root) do |root|
  home=File.join(root,'home'); gate=File.join(root,'gate')
  FileUtils.mkdir_p([gate]+%w[.config .cache .local/share .local/state .kde].map { |name| File.join(home,name) })
  daemon=ENV.fetch('PATH').split(':').map { |part| File.join(part,'dbus-daemon') }.find { |path| File.executable?(path) }
  upstream=File.join(root,'bus')
  upstream_error=File.open(File.join(evidence,'upstream.stderr'),'w')
  upstream_pid=Process.spawn(daemon,'--session','--nofork','--nopidfile',"--address=unix:path=#{upstream}",out:File::NULL,err:upstream_error)
  begin
    Timeout.timeout(5) { sleep 0.02 until File.socket?(upstream) }
    env={'HOME'=>home,'XDG_CONFIG_HOME'=>"#{home}/.config",'XDG_CACHE_HOME'=>"#{home}/.cache",'XDG_DATA_HOME'=>"#{home}/.local/share",'XDG_STATE_HOME'=>"#{home}/.local/state",'KDEHOME'=>"#{home}/.kde",
      'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{upstream}",'KWIN_MCP_REPO'=>"#{home}/no-source",'CARGO'=>'/usr/bin/false','KWIN_MCP_TEST_SCRIPT_CLEANUP_GATE'=>gate}
    errors=File.open(File.join(evidence,'server.stderr'),'w')
    log=File.open(File.join(evidence,'rpc.jsonl'),'w')
    input,output,process=Open3.popen2(env,server,'--autoclean',err:errors)
    id=0; compositor=nil; paused=false; reader=nil
    rpc=lambda do |method,params|
      id+=1;input.puts(JSON.generate(jsonrpc:'2.0',id:id,method:method,params:params));input.flush
      started=Process.clock_gettime(Process::CLOCK_MONOTONIC)
      response=Timeout.timeout(40) do
        loop do
          line=output.gets or raise 'server EOF'
          value=JSON.parse(line)
          break value if value['id']==id
        end
      end
      log.puts(JSON.generate(method:method,params:params,elapsed_seconds:Process.clock_gettime(Process::CLOCK_MONOTONIC)-started,response:response));log.flush
      raise response.inspect if response['error'] || response.dig('result','isError')
      response.fetch('result')
    end
    begin
      rpc.call('initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'script-cleanup-proof',version:'1'})
      input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));input.flush
      started=rpc.call('tools/call',name:'session_start',arguments:{width:800,height:600,browser_profiles:'fresh'}).fetch('structuredContent')
      session=started.fetch('session_id');workdir=started.fetch('workdir')
      list=rpc.call('tools/call',name:'session_list',arguments:{}).fetch('structuredContent').fetch('sessions')
      sandbox=list.find { |entry| entry['session_id']==session }.fetch('sandbox_pid')
      compositor=compositor_under(sandbox,workdir)
      result=nil
      began=Process.clock_gettime(Process::CLOCK_MONOTONIC)
      reader=Thread.new { result=rpc.call('tools/call',name:'window_list',arguments:{session_id:session}) }
      Timeout.timeout(15) { sleep 0.01 until File.file?(File.join(gate,'ready.json')) }
      control=JSON.parse(File.read(File.join(gate,'ready.json')))
      Process.kill('STOP',compositor);paused=true
      Timeout.timeout(2) { sleep 0.01 until File.read("/proc/#{compositor}/status").match?(/^State:\s+T/) }
      File.write(File.join(gate,'release'),'continue cleanup')
      sleep 2
      Process.kill('CONT',compositor);paused=false
      reader.value
      elapsed=Process.clock_gettime(Process::CLOCK_MONOTONIC)-began
      raise 'real window list result missing' unless result.fetch('structuredContent').fetch('windows').is_a?(Array)
      bus="unix:path=#{workdir}/bus"
      loaded,status=Open3.capture2e('gdbus','call','--address',bus,'--dest',control.fetch('kwin_name'),'--object-path','/Scripting','--method','org.kde.kwin.Scripting.isScriptLoaded',control.fetch('marker'))
      raise "script still loaded: #{loaded}" unless status.success? && loaded.include?('false')
      callback,status=Open3.capture2e('gdbus','introspect','--address',bus,'--dest',control.fetch('callback_name'),'--object-path',control.fetch('callback_path'))
      raise "callback still exported: #{callback}" if status.success? && callback.include?('org.kde.KWinMCP')
      scripts=File.join(home,'.cache/kwin-mcp',File.basename(workdir),'kwin-scripts')
      raise 'script file retained' unless Dir.glob(File.join(scripts,'*.js')).empty?
      stopped=rpc.call('tools/call',name:'session_stop',arguments:{session_id:session}).fetch('structuredContent')
      raise "stop failed: #{stopped}" unless stopped['status']=='stopped'
      raise 'workdir retained' if File.exist?(workdir)
      proof={pause_seconds:2,window_list_elapsed_seconds:elapsed,script_unloaded:true,callback_removed:true,script_file_removed:true,session_cleanup:true}
      File.write(File.join(evidence,'results.json'),JSON.pretty_generate(proof));puts JSON.generate(proof)
    ensure
      Process.kill('CONT',compositor) if paused && compositor
      File.write(File.join(gate,'release'),'release on failure')
      input.close
      begin
        Timeout.timeout(30) { process.value }
      rescue Timeout::Error
        Process.kill('TERM',process.pid)
        Timeout.timeout(30) { process.value }
      ensure
        reader&.join(2)
        output.close;errors.close;log.close
      end
    end
  ensure
    Process.kill('TERM',upstream_pid) rescue Errno::ESRCH
    Process.wait(upstream_pid)
    upstream_error.close
  end
end
