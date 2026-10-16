#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'rbconfig'

binary, evidence = ARGV
abort 'usage: client_lifetime_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
bus = ENV.fetch("KWIN_MCP_E2E_SESSION_BUS", "unix:path=/run/user/#{Process.euid}/bus")
runtime = "/run/user/#{Process.euid}"

if ARGV[2]&.start_with?('controller-')
  mode = ARGV[2].delete_prefix('controller-')
  home = File.join(evidence,'home')
  %w[.config .cache .local/state .local/share .kde].each { |name| FileUtils.mkdir_p(File.join(home,name)) }
  env = {'HOME'=>home,'XDG_CONFIG_HOME'=>File.join(home,'.config'),'XDG_CACHE_HOME'=>File.join(home,'.cache'),
         'XDG_STATE_HOME'=>File.join(home,'.local/state'),'XDG_DATA_HOME'=>File.join(home,'.local/share'),
         'KDEHOME'=>File.join(home,'.kde'),'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus,
         'KWIN_MCP_REPO'=>File.join(evidence,'no-repository'),'CARGO'=>'/usr/bin/false'}
  input, output, error, server = Open3.popen3(env,binary,'--autoclean','--width','800','--height','600')
  diagnostics = Thread.new { File.open(File.join(evidence,'server.stderr'),'w') { |log| IO.copy_stream(error,log) } }
  id = 0
  reloaded = false
  trace = File.open(File.join(evidence,'rpc.jsonl'),'w')
  rpc = lambda do |method,params|
    id += 1
    input.puts(JSON.generate(jsonrpc:'2.0',id:id,method:method,params:params)); input.flush
    Timeout.timeout(60) do
      loop do
        line = output.gets or raise 'server EOF'
        reply = JSON.parse(line)
        trace.puts(JSON.generate(reply)); trace.flush
        reloaded = true if reply['method'] == 'notifications/tools/list_changed'
        next unless reply['id'] == id
        raise reply.inspect if reply['error'] || reply.dig('result','isError')
        break reply.fetch('result')
      end
    end
  end
  rpc.call('initialize',{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'client-lifetime-proof',version:'1'}})
  input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized')); input.flush
  2.times { rpc.call('tools/call',{name:'session_start',arguments:{}}) }
  sessions = rpc.call('tools/call',{name:'session_list',arguments:{}}).dig('structuredContent','sessions')
  raise 'expected two owned sessions' unless sessions.length == 2
  records = sessions.map do |session|
    unit = session['scope_unit']
    if !unit || unit.empty?
      cgroup = File.read("/proc/#{session.fetch('sandbox_pid')}/cgroup").lines.find { |line| line.start_with?('0::') }.split('::',2).last.strip
      unit = File.basename(cgroup)
    end
    group, status = Open3.capture2({'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus},'systemctl','--user','show','-p','ControlGroup','--value',unit)
    raise 'missing live scope' unless status.success? && !group.strip.empty?
    cgroup = "/sys/fs/cgroup#{group.strip}"
    pids = Dir.glob(File.join(cgroup,'**','cgroup.procs')).flat_map { |file| File.readlines(file).map(&:to_i) }.uniq
    names = pids.filter_map do |pid|
      [pid,File.read("/proc/#{pid}/comm").strip]
    rescue Errno::ENOENT
      nil
    end
    raise 'scope omitted nested KWin' unless names.any? { |_,name| name == 'kwin_wayland' }
    raise 'scope omitted D-Bus' unless names.any? { |_,name| name == 'dbus-daemon' }
    disk = File.join(home, '.cache/kwin-mcp', File.basename(session.fetch('workdir')))
    raise 'session disk overlay missing before client exit' unless File.directory?(disk)
    session.merge('scope_unit'=>unit,'cgroup'=>cgroup,'members'=>names,'disk'=>disk)
  end
  keeper = mode == 'eof' ? nil : Process.spawn('sleep','300',3=>input,close_others:true,out:File::NULL,err:File::NULL)
  ready = {controller:Process.pid,server:server.pid,keeper:keeper,sessions:records}
  File.write(File.join(evidence,'ready.pending'),JSON.pretty_generate(ready))
  File.rename(File.join(evidence,'ready.pending'),File.join(evidence,'ready.json'))
  if mode == 'eof'
    sleep 0.05 until File.exist?(File.join(evidence,'action'))
    input.close
    Timeout.timeout(20) { server.value }
    diagnostics.join
    File.write(File.join(evidence,'eof-done'),'server exited')
  elsif %w[reload upgrade].include?(mode)
    sleep 0.05 until File.exist?(File.join(evidence,'action'))
    after = nil
    Timeout.timeout(30) do
      loop do
        after = rpc.call('tools/call',{name:'session_list',arguments:{}}).dig('structuredContent','sessions')
        break if reloaded
        sleep 0.1
      end
    end
    raise 'reload changed session identities' unless after.map { |session| session['session_id'] }.sort == sessions.map { |session| session['session_id'] }.sort
    raise 'reload lost scope identities' unless after.map { |session| session['scope_unit'] }.sort == records.map { |session| session['scope_unit'] }.sort
    File.write(File.join(evidence,'reload-proof.json'),JSON.pretty_generate(server_pid:server.pid,sessions:after))
  end
  sleep 300
  exit
end

controller = nil
record = nil
mode = ARGV[2] || 'kill'
abort 'mode must be kill, eof, reload, or upgrade' unless %w[kill eof reload upgrade].include?(mode)
replacement = ARGV[3] || binary
if %w[reload upgrade].include?(mode)
  private_binary = File.join(evidence,'kwin-mcp')
  FileUtils.cp(binary,private_binary)
  binary = private_binary
end
begin
  controller = Process.spawn(RbConfig.ruby,__FILE__,binary,evidence,"controller-#{mode}",out:File.join(evidence,'controller.stdout'),err:File.join(evidence,'controller.stderr'))
  Timeout.timeout(90) do
    sleep 0.05 until File.exist?(File.join(evidence,'ready.json'))
  end
  record = JSON.parse(File.read(File.join(evidence,'ready.json')))
  raise 'controller identity mismatch' unless record['controller'] == controller
  if %w[reload upgrade].include?(mode)
    FileUtils.cp(replacement,"#{binary}.next")
    File.rename("#{binary}.next",binary)
    File.write(File.join(evidence,'action'),'reload')
    Timeout.timeout(40) { sleep 0.05 until File.exist?(File.join(evidence,'reload-proof.json')) }
  end
  if mode == 'eof'
    File.write(File.join(evidence,'action'),'close stdin')
    Timeout.timeout(25) { sleep 0.05 until File.exist?(File.join(evidence,'eof-done')) }
  else
    Process.kill('KILL',controller)
    Process.wait(controller)
    controller = nil
    raise 'inherited writer already ended' unless File.directory?("/proc/#{record.fetch('keeper')}")
  end
  before = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  Timeout.timeout(20) do
    loop do
      processes_gone = !File.directory?("/proc/#{record.fetch('server')}") || File.read("/proc/#{record.fetch('server')}/stat").split[2] == 'Z'
      scopes_empty = record.fetch('sessions').all? do |session|
        Dir.glob(File.join(session.fetch('cgroup'),'**','cgroup.procs')).all? { |file| File.read(file).strip.empty? }
      end
      workdirs_gone = record.fetch('sessions').all? { |session| !File.exist?(session.fetch('workdir')) && !File.exist?(session.fetch('disk')) }
      break if processes_gone && scopes_empty && workdirs_gone
      sleep 0.05
    end
  end
  # Independent saved-member identities must also be absent or reaped zombies.
  record.fetch('sessions').each do |session|
    session.fetch('members').each do |pid,name|
      next unless File.exist?("/proc/#{pid}/stat")
      state = File.read("/proc/#{pid}/stat").split[2]
      raise "scope member survived: #{pid} #{name} #{state}" unless state == 'Z'
    end
    group, = Open3.capture2({'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus},'systemctl','--user','show','-p','ControlGroup','--value',session.fetch('scope_unit'))
    raise 'systemd still reports a populated scope' unless group.strip.empty?
  end
  raise 'writer was killed with session' if record['keeper'] && !File.directory?("/proc/#{record.fetch('keeper')}")
  elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-before
  File.write(File.join(evidence,'verification.json'),JSON.pretty_generate(status:'passed',mode:mode,elapsed:elapsed,writer_still_alive:!record['keeper'].nil?,record:record))
  puts "client_#{mode}=PASS (#{elapsed.round(3)}s; two scopes, members terminated and tmp/disk storage gone)"
rescue Timeout::Error
  if record
    observed = {
      server_present: File.directory?("/proc/#{record.fetch('server')}"),
      keeper_present: record['keeper'] && File.directory?("/proc/#{record['keeper']}"),
      sessions: record.fetch('sessions').map do |session|
        session.merge('workdir_present'=>File.exist?(session.fetch('workdir')),
                      'disk_present'=>File.exist?(session.fetch('disk')),
                      'remaining_members'=>Dir.glob(File.join(session.fetch('cgroup'),'**','cgroup.procs')).flat_map { |file| File.readlines(file).map(&:to_i) })
      end,
    }
    File.write(File.join(evidence,'timeout-observation.json'),JSON.pretty_generate(observed))
  end
  raise
ensure
  if controller
    Process.kill('TERM',controller) rescue Errno::ESRCH
    Process.wait(controller) rescue Errno::ECHILD
  end
  if record
    Process.kill('TERM',record.fetch('keeper')) rescue Errno::ESRCH if record['keeper']
    if File.exist?("/proc/#{record.fetch('server')}/stat") && File.read("/proc/#{record.fetch('server')}/stat").split[2] != 'Z'
      Process.kill('TERM',record.fetch('server')) rescue Errno::ESRCH
    end
    record.fetch('sessions').each do |session|
      system({'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus},'systemctl','--user','stop',session.fetch('scope_unit'),out:File::NULL,err:File::NULL)
    end
  end
end
