#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'rbconfig'

binary, evidence = ARGV
abort 'usage: session_stop_scope_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
runtime = "/run/user/#{Process.euid}"
bus = "unix:path=#{runtime}/bus"
home = File.join(evidence, 'home')
%w[.config .cache .local/state .local/share .kde].each do |name|
  FileUtils.mkdir_p(File.join(home, name))
end

def process_identity(pid)
  stat = File.read("/proc/#{pid}/stat")
  command_end = stat.rindex(')')
  fields = stat[(command_end + 2)..].split
  {state: fields[0], start_time: fields[19]}
rescue Errno::ENOENT
  nil
end

def process_running?(pid)
  identity = process_identity(pid)
  identity && identity[:state] != 'Z'
end

def rpc(input, output, trace, id, method, params)
  input.puts(JSON.generate(jsonrpc: '2.0', id: id, method: method, params: params))
  input.flush
  Timeout.timeout(90) do
    loop do
      line = output.gets or raise 'kwin-mcp closed its JSON-RPC output'
      reply = JSON.parse(line)
      trace.puts(JSON.generate(reply))
      trace.flush
      next unless reply['id'] == id
      raise reply.inspect if reply['error']
      result = reply.fetch('result')
      raise result.inspect if result['isError']
      return result
    end
  end
end

def tool(input, output, trace, id, name, arguments = {})
  rpc(input, output, trace, id, 'tools/call', {name: name, arguments: arguments})
end

def scope_pids(cgroup)
  Dir.glob(File.join(cgroup, '**', 'cgroup.procs')).flat_map do |file|
    File.readlines(file).map(&:to_i)
  end.uniq
end

Signal.trap('TERM') { $term_seen = true }
adapter_ready = File.join(evidence, 'host-adapter-ready')
adapter_term = File.join(evidence, 'host-adapter-term')
adapter_code = <<~'RUBY'
  require 'socket'
  listener = TCPServer.new('127.0.0.1', 0)
  File.write(ARGV.fetch(0), listener.addr[1].to_s)
  trap('TERM') { File.write(ARGV.fetch(1), 'SIGTERM'); exit 143 }
  loop do
    client = listener.accept
    client.close
  end
RUBY
adapter = Process.spawn(RbConfig.ruby, '-e', adapter_code, adapter_ready, adapter_term,
                        out: File::NULL, err: File::NULL)
input = output = error = server = trace = nil
session_id = nil
server_log = File.join(evidence, 'server.stderr')

begin
  Timeout.timeout(10) { sleep 0.05 until File.file?(adapter_ready) }
  adapter_port = Integer(File.read(adapter_ready))
  env = {
    'HOME' => home,
    'XDG_CONFIG_HOME' => File.join(home, '.config'),
    'XDG_CACHE_HOME' => File.join(home, '.cache'),
    'XDG_STATE_HOME' => File.join(home, '.local/state'),
    'XDG_DATA_HOME' => File.join(home, '.local/share'),
    'KDEHOME' => File.join(home, '.kde'),
    'XDG_RUNTIME_DIR' => runtime,
    'DBUS_SESSION_BUS_ADDRESS' => bus,
    'KWIN_MCP_REPO' => File.join(evidence, 'no-repository'),
    'CARGO' => '/usr/bin/false'
  }
  input, output, error, server = Open3.popen3(
    env, binary, '--autoclean', '--no-viewer', '--width', '800', '--height', '600'
  )
  diagnostics = Thread.new do
    File.open(server_log, 'w') { |log| IO.copy_stream(error, log) }
  end
  trace = File.open(File.join(evidence, 'rpc.jsonl'), 'w')
  rpc(input, output, trace, 1, 'initialize', {
    protocolVersion: '2024-11-05', capabilities: {},
    clientInfo: {name: 'session-stop-scope-proof', version: '1'}
  })
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized', params: {}))
  input.flush

  started = tool(input, output, trace, 2, 'session_start')
  session_id = started.dig('structuredContent', 'session_id')
  raise "session_start did not return a session ID: #{started}" unless session_id
  tool(input, output, trace, 3, 'launch_app', {
    command: 'konsole --hold -e sleep 300',
    window_timeout_seconds: 30
  })
  sessions = tool(input, output, trace, 5, 'session_list')
            .dig('structuredContent', 'sessions')
  session = sessions.find { |entry| entry['session_id'] == session_id }
  raise "session_list omitted the owned session: #{sessions}" unless session
  sandbox_pid = session.fetch('sandbox_pid')
  unit = session.fetch('scope_unit')
  host_cgroup = File.read("/proc/#{adapter}/cgroup")
  caller_group = Process.getpgrp
  raise 'host adapter is not in the host client process group' unless Process.getpgid(adapter) == caller_group
  raise 'MCP server is not in the host client process group' unless Process.getpgid(server.pid) == caller_group
  raise 'sandbox did not start in its own process group' unless Process.getpgid(sandbox_pid) == sandbox_pid

  manager_env = {'XDG_RUNTIME_DIR' => runtime, 'DBUS_SESSION_BUS_ADDRESS' => bus}
  group, status = Open3.capture2(manager_env, 'systemctl', '--user', 'show',
                                 '-p', 'ControlGroup', '--value', unit)
  raise "could not inspect session scope #{unit}: #{group}" unless status.success? && !group.strip.empty?
  cgroup = File.join('/sys/fs/cgroup', group.strip.sub(%r{\A/}, ''))
  before_pids = scope_pids(cgroup)
  before_members = before_pids.filter_map do |pid|
    identity = process_identity(pid)
    {pid: pid, start_time: identity[:start_time]} if identity
  end
  raise "sandbox process #{sandbox_pid} is missing from its scope #{unit}" unless before_pids.include?(sandbox_pid)
  raise 'host adapter entered the session scope' if before_pids.include?(adapter)
  raise 'MCP server entered the session scope' if before_pids.include?(server.pid)
  raise 'session-stop scope was already signaled before stop' if File.exist?(adapter_term)

  app_names = before_pids.filter_map do |pid|
    File.read("/proc/#{pid}/comm").strip
  rescue Errno::ENOENT
    nil
  end
  raise "Konsole was not launched in the owned session scope: #{app_names}" unless app_names.include?('konsole')

  stopped = tool(input, output, trace, 6, 'session_stop', {session_id: session_id})
  raise "session_stop did not report stopped: #{stopped}" unless stopped.dig('structuredContent', 'status') == 'stopped'
  stopped_session_id = session_id
  session_id = nil
  remaining = tool(input, output, trace, 7, 'session_list')
              .dig('structuredContent', 'sessions')
  raise "session_stop left an owned session: #{remaining}" unless remaining.empty?
  raise 'session_stop signaled its MCP client process group' if $term_seen
  raise 'host adapter received SIGTERM during session_stop' if File.exist?(adapter_term)
  raise 'host adapter did not survive session_stop' unless process_running?(adapter)
  raise 'MCP server did not survive session_stop' unless process_running?(server.pid)
  surviving_members = before_members.filter_map do |member|
    identity = process_identity(member[:pid])
    member[:pid] if identity && identity[:start_time] == member[:start_time] && identity[:state] != 'Z'
  end
  raise "session processes survived stop outside the scope: #{surviving_members}" unless surviving_members.empty?

  after_group, after_status = Open3.capture2(manager_env, 'systemctl', '--user', 'show',
                                             '-p', 'ControlGroup', '--value', unit)
  raise "could not recheck session scope #{unit}: #{after_group}" unless after_status.success?
  unless after_group.strip.empty?
    after_cgroup = File.join('/sys/fs/cgroup', after_group.strip.sub(%r{\A/}, ''))
    remaining_pids = scope_pids(after_cgroup)
    raise "session processes survived stop in #{unit}: #{remaining_pids}" unless remaining_pids.empty?
  end
  File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(
    status: 'passed', session_id: stopped_session_id, sandbox_pid: sandbox_pid,
    scope_unit: unit, scope_members_before_stop: before_pids,
    scope_members_after_stop: [], host_adapter_pid: adapter,
    host_adapter_cgroup: host_cgroup, host_adapter_alive: true,
    host_adapter_port: adapter_port,
    mcp_server_pid: server.pid, mcp_server_alive: true,
    host_client_process_group: caller_group, sandbox_process_group: sandbox_pid
  ))
  puts "session_stop_scope=PASS (#{unit} stopped; host adapter #{adapter} and MCP server #{server.pid} survived)"
ensure
  if input && output && trace && session_id && server && process_running?(server.pid)
    begin
      tool(input, output, trace, 99, 'session_stop', {session_id: session_id})
    rescue StandardError
      nil
    end
  end
  [input, output, trace].compact.each { |io| io.close rescue nil }
  if server
    Process.kill('TERM', server.pid) if process_running?(server.pid)
    Timeout.timeout(10) { server.value } rescue Timeout::Error
  end
  if adapter
    Process.kill('TERM', adapter) if process_running?(adapter)
    Timeout.timeout(3) { Process.wait(adapter) } rescue Timeout::Error
  end
  diagnostics.join if defined?(diagnostics) && diagnostics
end
