#!/usr/bin/env ruby
# Trace real stdio startup/stop with synthetic cookie jars and no browser credentials.
require 'sqlite3'
require 'json'
require 'open3'
require 'fileutils'
require 'tmpdir'
require 'timeout'
require 'digest'

File.umask(0o077)
server = File.realpath(ARGV.fetch(0))
evidence = File.expand_path(ARGV.fetch(1))
FileUtils.mkdir_p(evidence)
rows = Integer(ENV.fetch('KWIN_MCP_COOKIE_BATCH_ROWS','500'))
abort 'fixture needs at least 200 rows' unless rows >= 200
baseline = ENV['KWIN_MCP_E2E_BASELINE'] == '1'
daemon = ENV.fetch('PATH').split(':').map { |part| File.join(part,'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon

def seed_jar(path, count)
  FileUtils.mkdir_p(File.dirname(path))
  db = SQLite3::Database.new(path)
  db.execute_batch('CREATE TABLE cookies(host_key TEXT NOT NULL,name TEXT NOT NULL,path TEXT NOT NULL,last_update_utc INTEGER NOT NULL,expires_utc INTEGER NOT NULL,has_expires INTEGER NOT NULL,encrypted_value BLOB NOT NULL); CREATE UNIQUE INDEX cookie_identity ON cookies(host_key,name,path);')
  db.transaction do
    count.times do |index|
      db.execute("INSERT INTO cookies VALUES(?,?,?, ?,0,0,x'0011')",['.example.test',"key#{index}",'/',index==count-1 ? 200 : 100])
    end
    db.execute("INSERT INTO cookies VALUES('.example.test','keep','/',500,0,0,x'0011')")
  end
ensure
  db&.close
end

def jar_rows(path)
  db = SQLite3::Database.new(path)
  db.execute('SELECT name,last_update_utc FROM cookies ORDER BY name')
ensure
  db&.close
end

Dir.mktmpdir('kmcb-','/var/tmp') do |root|
  home = File.join(root,'home')
  FileUtils.mkdir_p(%w[.config .cache .local/share .local/state .kde].map { |name| File.join(home,name) })
  relative = '.config/google-chrome/Default/Cookies'
  host = File.join(home,relative)
  saved = File.join(home,'.local/state/kwin-mcp/browser-cookies',relative)
  seed_jar(host,rows)
  FileUtils.mkdir_p(File.dirname(saved))
  tombstones = {columns:%w[host_key name path], rows:rows.times.map { |index| {key:[{Text:'.example.test'},{Text:"key#{index}"},{Text:'/'}],through:100} }}
  File.write(saved.sub(/Cookies\z/,'Cookies.deleted.json'),JSON.generate(tombstones))
  host_hash = Digest::SHA256.file(host).hexdigest
  upstream_path = File.join(root,'upstream')
  upstream_error = File.open(File.join(evidence,'upstream.stderr'),'w')
  upstream_pid = Process.spawn(daemon,'--session','--nofork','--nopidfile',"--address=unix:path=#{upstream_path}",out:File::NULL,err:upstream_error)
  begin
    Timeout.timeout(5) { sleep 0.02 until File.socket?(upstream_path) }
    env = {'HOME'=>home,'XDG_CONFIG_HOME'=>"#{home}/.config",'XDG_CACHE_HOME'=>"#{home}/.cache",
      'XDG_DATA_HOME'=>"#{home}/.local/share",'XDG_STATE_HOME'=>"#{home}/.local/state",'KDEHOME'=>"#{home}/.kde",
      'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{upstream_path}",
      'KWIN_MCP_REPO'=>"#{home}/no-source",'CARGO'=>'/usr/bin/false','DISPLAY'=>nil,'WAYLAND_DISPLAY'=>nil,'WAYLAND_SOCKET'=>nil}
    errors = File.open(File.join(evidence,'server.stderr'),'w')
    trace_path = File.join(evidence,'durability.strace')
    input,output,process = Open3.popen2(env,'strace','-D','-f','-yy','-e','trace=fsync,fdatasync','-o',trace_path,server,'--autoclean',err:errors)
    log = File.open(File.join(evidence,'rpc.jsonl'),'w')
    id = 0
    session = nil
    times = {}
    rpc = lambda do |method,params|
      id+=1;input.puts(JSON.generate(jsonrpc:'2.0',id:id,method:method,params:params));input.flush
      began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      response = Timeout.timeout(120) do
        loop do
          line = output.gets or raise 'server EOF'
          parsed = JSON.parse(line)
          break parsed if parsed['id']==id
        end
      end
      elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-began
      log.puts(JSON.generate(method:method,params:params,elapsed_seconds:elapsed,response:response));log.flush
      times[params[:name]] = elapsed if params[:name]
      raise response.inspect if response['error'] || response.dig('result','isError')
      response.fetch('result')
    end
    begin
      rpc.call('initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'cookie-batch-proof',version:'1'})
      input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized'));input.flush
      started = rpc.call('tools/call',name:'session_start',arguments:{width:800,height:600}).fetch('structuredContent')
      session = started.fetch('session_id')
      workdir = started.fetch('workdir')
      raise 'real compositor socket missing' unless File.socket?(File.join(workdir,'wayland-0'))
      rpc.call('tools/call',name:'window_list',arguments:{session_id:session})
      exported = File.join(evidence,'started.sqlite')
      rpc.call('tools/call',name:'export_file',arguments:{session_id:session,session_path:host,host_path:exported})
      expected = [["key#{rows-1}",200],['keep',500]].sort
      raise 'startup tombstones/newer update semantics differ' unless jar_rows(exported).sort == expected
      # Reintroduce stale saved rows so stop also has to apply the deletion set.
      FileUtils.cp(host,saved)
      stopped = rpc.call('tools/call',name:'session_stop',arguments:{session_id:session}).fetch('structuredContent')
      session = nil
      raise "stop failed: #{stopped}" unless stopped['status']=='stopped'
      raise 'saved tombstones/newer update semantics differ' unless jar_rows(saved).sort == expected
      raise 'host jar changed' unless Digest::SHA256.file(host).hexdigest == host_hash
      disk = File.join(home,'.cache/kwin-mcp',File.basename(workdir))
      raise 'owned storage retained' if File.exist?(workdir) || File.exist?(disk)
    ensure
      rpc.call('tools/call',name:'session_stop',arguments:{session_id:session}) if session
      input.close
      begin
        Timeout.timeout(60) { process.value }
      ensure
        output.close;errors.close;log.close
      end
    end
    syncs = File.readlines(trace_path).count { |line| line.match?(/\b(?:fdatasync|fsync)\(/) && line.include?('/Cookies') }
    raise "baseline did not expose per-row syncs: #{syncs}" if baseline && syncs < rows
    raise "per-row durable commits remain: #{syncs}" if !baseline && syncs >= 100
    proof = {rows:rows,cookie_durable_syncs:syncs,startup_seconds:times['session_start'],stop_seconds:times['session_stop'],baseline:baseline,
      newer_update_preserved:true,deleted_rows_removed:true,host_unchanged:true,owned_storage_removed:true}
    File.write(File.join(evidence,'results.json'),JSON.pretty_generate(proof))
    puts JSON.generate(proof)
  ensure
    Process.kill('TERM',upstream_pid) rescue Errno::ESRCH
    Process.wait(upstream_pid)
    upstream_error.close
  end
end
