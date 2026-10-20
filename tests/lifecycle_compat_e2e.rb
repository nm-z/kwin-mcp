#!/usr/bin/env ruby
# Real old/new stdio readers, versioned history, and live handles across a failed start.
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'
require 'tmpdir'

server=File.realpath(ARGV.fetch(0));old_server=File.realpath(ARGV.fetch(1));evidence=File.expand_path(ARGV.fetch(2))
FileUtils.mkdir_p(evidence)

class LifecycleClient
  def initialize(binary,env,evidence,name)
    @log=File.open(File.join(evidence,"#{name}.jsonl"),'w',0o600)
    @errors=File.open(File.join(evidence,"#{name}.stderr"),'w',0o600)
    @input,@output,@process=Open3.popen2(env,binary,'--autoclean',err:@errors)
    @id=0
    request('initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'lifecycle-compat-proof',version:'1'})
    @input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));@input.flush
  end
  def request(method,params)
    @id+=1;@input.puts(JSON.generate(jsonrpc:'2.0',id:@id,method:method,params:params));@input.flush
    response=Timeout.timeout(50) do
      loop do
        line=@output.gets or raise 'server EOF'
        value=JSON.parse(line)
        break value if value['id']==@id
      end
    end
    @log.puts(JSON.generate(method:method,params:params,response:response));@log.flush
    response
  end
  def call(name,args={});request('tools/call',name:name,arguments:args);end
  def data(response)
    raise response.inspect if response['error'] || response.dig('result','isError')
    response.fetch('result').fetch('structuredContent')
  end
  def close
    @input.close
    begin
      Timeout.timeout(30) { @process.value }
    rescue Timeout::Error
      Process.kill('TERM',@process.pid)
      Timeout.timeout(30) { @process.value }
    ensure
      @output.close;@errors.close;@log.close
    end
  end
end

def records(path)
  File.readlines(path).map { |line|JSON.parse(line) }
end

def legacy_valid?(r)
  r['state']=='shutdown_cleanup_finished' && r['reason']=='server_shutdown' && %w[not_requested removed retained].include?(r['workdir_cleanup']) &&
    r['server_pid'].positive? && r['finished_at_unix_seconds'].positive? && r['session_id'].match?(/\As#{r['server_pid']}-[1-9][0-9]*\z/)
end

Dir.mktmpdir('kmlc-','/var/tmp') do |root|
  home=File.join(root,'home');state=File.join(root,'state');runtime=File.join(root,'runtime')
  FileUtils.mkdir_p([state,runtime]+%w[.config .cache .local/share .local/state .kde].map { |name|File.join(home,name) })
  directory=File.join(state,'kwin-mcp');FileUtils.mkdir_p(directory)
  legacy=File.join(directory,'session-lifecycle.jsonl');current=File.join(directory,'session-lifecycle-v2.jsonl')
  pid_max=Integer(File.read('/proc/sys/kernel/pid_max'))
  make=lambda do |pid,event,reason,cleanup,finished|
    {session_id:"s#{pid}-1",server_pid:pid,state:event,reason:reason,workdir_cleanup:cleanup,finished_at_unix_seconds:finished}
  end
  old=make.call(pid_max+100,'shutdown_cleanup_finished','server_shutdown','removed',100)
  moved=make.call(pid_max+101,'session_stopped','session_stop','retained',200)
  confirmed=moved.merge(workdir_cleanup:'removed')
  future=make.call(pid_max+102,'future_terminal_kind','future_reason','future_cleanup_kind',300)
  original=[old,moved].map { |r|JSON.generate(r) }.join("\n")+"\n"
  File.write(legacy,original)
  File.write(current,[confirmed,future].map { |r|JSON.generate(r) }.join("\n")+"\n")
  pinned=File.join(root,'legacy-kwin-mcp');FileUtils.cp(old_server,pinned)
  version,status=Open3.capture2(pinned,'--version')
  raise "expected legacy16a7357 executable: #{version}" unless status.success? && version.include?('16a7357')
  daemon=ENV.fetch('PATH').split(':').map { |part|File.join(part,'dbus-daemon') }.find { |path|File.executable?(path) }
  upstream=File.join(root,'bus');upstream_log=File.open(File.join(evidence,'upstream.stderr'),'w')
  upstream_pid=Process.spawn(daemon,'--session','--nofork','--nopidfile',"--address=unix:path=#{upstream}",out:File::NULL,err:upstream_log)
  old_client=nil;modern=nil;preferences=nil
  begin
    Timeout.timeout(5) { sleep 0.02 until File.socket?(upstream) }
    env={'HOME'=>home,'XDG_CONFIG_HOME'=>"#{home}/.config",'XDG_CACHE_HOME'=>"#{home}/.cache",'XDG_DATA_HOME'=>"#{home}/.local/share",'XDG_STATE_HOME'=>state,'KDEHOME'=>"#{home}/.kde",
      'TMPDIR'=>runtime,'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{upstream}",'KWIN_MCP_REPO'=>"#{home}/no-source",'CARGO'=>'/usr/bin/false'}
    old_client=LifecycleClient.new(pinned,env,evidence,'old')
    broken=old_client.call('window_list',session_id:old.fetch(:session_id))
    raise "old reader did not expose schema incompatibility: #{broken}" unless broken.dig('error','message')&.include?('invalid session-lifecycle record fields')
    modern=LifecycleClient.new(server,env,evidence,'current')
    migrated=modern.call('window_list',session_id:moved.fetch(:session_id)).fetch('error').fetch('data')
    raise 'modern stopped record unavailable' unless migrated['session_status']=='stopped'
    raise 'migration undid a confirmed cleanup retry' unless migrated.dig('previous_session_lifecycle_record','workdir_cleanup')=='removed'
    raise 'legacy file still incompatible' unless records(legacy).all? { |r|legacy_valid?(r) }
    backup=File.join(directory,'session-lifecycle-v1-migration.jsonl')
    raise 'migration recovery copy differs' unless File.binread(backup)==original
    raise 'migration lost a newer event' unless records(current).any? { |r|r['session_id']==moved.fetch(:session_id) }
    recovered=old_client.call('window_list',session_id:old.fetch(:session_id))
    raise "old reader still rejects history: #{recovered}" unless recovered.dig('error','data','previous_server_cleanup')=='shutdown_cleanup_finished'
    unknown=modern.call('window_list',session_id:future.fetch(:session_id)).fetch('error').fetch('data')
    raise 'future terminal kind misclassified' unless unknown['session_status']=='unknown' && !unknown.key?('session_stopped_at_unix_seconds')
    sessions={}
    %w[A B].each do |letter|
      info=modern.data(modern.call('session_start',width:800,height:600))
      sessions[letter]=info.fetch('session_id')
      modern.data(modern.call('launch_app',session_id:info.fetch('session_id'),command:"printf '#{letter}' > \"$HOME/proof-#{letter}\"; kdialog --title lifecycle-#{letter} --msgbox '#{letter}'"))
    end
    preferences=File.join(home,'.config/google-chrome/Default/Preferences')
    FileUtils.mkdir_p(File.dirname(preferences))
    raise 'FIFO fixture failed' unless system('mkfifo',preferences)
    failed=modern.call('session_start',width:800,height:600,startup_timeout_seconds:3)
    error=failed.fetch('error').fetch('data')
    raise "did not block the real Preferences read: #{failed}" unless error['host_call_blocked']==true && error.fetch('stage').include?('read Preferences')
    failed_id=error.fetch('session_id')
    listed=modern.data(modern.call('session_list'))
    listing=listed.fetch('sessions')
    owned=Dir.glob(File.join(runtime,"kwin-mcp-#{listed.fetch('server_pid')}-*"))
    pending_paths=owned-listing.map { |entry|entry.fetch('workdir') }
    raise "pending startup storage missing: #{pending_paths}" unless pending_paths.length==1
    pending=pending_paths.first
    sessions.each do |letter,id|
      windows=modern.data(modern.call('window_list',session_id:id))
      raise 'earlier window handle lost' unless JSON.generate(windows).include?("lifecycle-#{letter}")
      exported=File.join(evidence,"proof-#{letter}.txt")
      modern.data(modern.call('export_file',session_id:id,session_path:File.join(home,"proof-#{letter}"),host_path:exported))
      raise 'earlier exported file differs' unless File.read(exported)==letter
    end
    Timeout.timeout(10) do
      loop do
        begin
          File.open(preferences,File::WRONLY|File::NONBLOCK) { |file|file.write('{}') }
          break
        rescue Errno::ENXIO
          sleep 0.05
        end
      end
    end
    Timeout.timeout(10) { sleep 0.05 while File.exist?(pending) }
    modern.data(modern.call('session_stop',session_id:failed_id))
    sessions.each_value { |id|modern.data(modern.call('session_stop',session_id:id)) }
    raise 'new writes contaminated legacy history' unless records(legacy).all? { |r|legacy_valid?(r) }
    sessions.each_value do |id|
      ended=modern.call('window_list',session_id:id).fetch('error').fetch('data')
      raise 'explicit stop missing from v2 history' unless ended['session_status']=='stopped' && ended.dig('previous_session_lifecycle_record','workdir_cleanup')=='removed'
    end
    again=old_client.call('window_list',session_id:old.fetch(:session_id))
    raise 'old reader regressed after new writes' unless again.dig('error','data','previous_server_cleanup')=='shutdown_cleanup_finished'
    [legacy,current,backup].each { |path|raise 'unbounded history' if File.size(path)>1024*1024 }
    File.write(File.join(evidence,'results.json'),JSON.pretty_generate(legacy_reader_recovered:true,records_preserved:true,future_kind_safe:true,cleanup_retry_preserved:true,
      earlier_windows_survive_failed_start:true,earlier_exports_survive_failed_start:true,new_records_versioned:true))
    puts File.read(File.join(evidence,'results.json'))
  ensure
    if preferences && File.pipe?(preferences)
      begin
        File.open(preferences,File::WRONLY|File::NONBLOCK) { |file|file.write('{}') }
      rescue Errno::ENXIO
      end
    end
    modern&.close;old_client&.close
    Process.kill('TERM',upstream_pid) rescue Errno::ESRCH
    Process.wait(upstream_pid);upstream_log.close
  end
end
