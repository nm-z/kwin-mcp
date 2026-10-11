#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'

binary, evidence = ARGV
abort 'usage: client_lifetime_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
bus = "unix:path=/run/user/#{Process.euid}/bus"
runtime = "/run/user/#{Process.euid}"

if ARGV[2] == 'controller'
  home = File.join(evidence,'home')
  %w[.config .cache .local/state .local/share .kde].each { |name| FileUtils.mkdir_p(File.join(home,name)) }
  env = {'HOME'=>home,'XDG_CONFIG_HOME'=>File.join(home,'.config'),'XDG_CACHE_HOME'=>File.join(home,'.cache'),
         'XDG_STATE_HOME'=>File.join(home,'.local/state'),'XDG_DATA_HOME'=>File.join(home,'.local/share'),
         'KDEHOME'=>File.join(home,'.kde'),'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus,
         'KWIN_MCP_REPO'=>File.join(evidence,'no-repository'),'CARGO'=>'/usr/bin/false'}
  input, output, error, server = Open3.popen3(env,binary,'--autoclean','--width','800','--height','600')
  diagnostics = Thread.new { File.write(File.join(evidence,'server.stderr'),error.read) }
  id = 0
  trace = File.open(File.join(evidence,'rpc.jsonl'),'w')
  rpc = lambda do |method,params|
    id += 1
    input.puts(JSON.generate(jsonrpc:'2.0',id:id,method:method,params:params)); input.flush
    Timeout.timeout(60) do
      loop do
        line = output.gets or raise 'server EOF'
        reply = JSON.parse(line)
        trace.puts(JSON.generate(reply)); trace.flush
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
    session.merge('scope_unit'=>unit,'cgroup'=>cgroup,'members'=>names)
  end
  keeper = Process.spawn('sleep','300',3=>input,close_others:true,out:File::NULL,err:File::NULL)
  ready = {controller:Process.pid,server:server.pid,keeper:keeper,sessions:records}
  File.write(File.join(evidence,'ready.pending'),JSON.pretty_generate(ready))
  File.rename(File.join(evidence,'ready.pending'),File.join(evidence,'ready.json'))
  sleep 300
  exit
end

controller = nil
record = nil
begin
  controller = Process.spawn(RbConfig.ruby,__FILE__,binary,evidence,'controller',out:File.join(evidence,'controller.stdout'),err:File.join(evidence,'controller.stderr'))
  Timeout.timeout(90) do
    sleep 0.05 until File.exist?(File.join(evidence,'ready.json'))
  end
  record = JSON.parse(File.read(File.join(evidence,'ready.json')))
  raise 'controller identity mismatch' unless record['controller'] == controller
  Process.kill('KILL',controller)
  Process.wait(controller)
  controller = nil
  raise 'inherited writer already ended' unless File.directory?("/proc/#{record.fetch('keeper')}")
  before = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  Timeout.timeout(20) do
    loop do
      processes_gone = !File.directory?("/proc/#{record.fetch('server')}") || File.read("/proc/#{record.fetch('server')}/stat").split[2] == 'Z'
      scopes_empty = record.fetch('sessions').all? do |session|
        Dir.glob(File.join(session.fetch('cgroup'),'**','cgroup.procs')).all? { |file| File.read(file).strip.empty? }
      end
      workdirs_gone = record.fetch('sessions').all? { |session| !File.exist?(session.fetch('workdir')) }
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
  raise 'writer was killed with session' unless File.directory?("/proc/#{record.fetch('keeper')}")
  elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-before
  File.write(File.join(evidence,'verification.json'),JSON.pretty_generate(status:'passed',elapsed:elapsed,writer_still_alive:true,record:record))
  puts "client_sigkill_with_inherited_writer=PASS (#{elapsed.round(3)}s; two scopes, all members and workdirs gone)"
ensure
  if controller
    Process.kill('TERM',controller) rescue Errno::ESRCH
    Process.wait(controller) rescue Errno::ECHILD
  end
  if record
    Process.kill('TERM',record.fetch('keeper')) rescue Errno::ESRCH
    if File.exist?("/proc/#{record.fetch('server')}/stat") && File.read("/proc/#{record.fetch('server')}/stat").split[2] != 'Z'
      Process.kill('TERM',record.fetch('server')) rescue Errno::ESRCH
    end
    record.fetch('sessions').each do |session|
      system({'XDG_RUNTIME_DIR'=>runtime,'DBUS_SESSION_BUS_ADDRESS'=>bus},'systemctl','--user','stop',session.fetch('scope_unit'),out:File::NULL,err:File::NULL)
    end
  end
end
