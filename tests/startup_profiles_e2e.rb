#!/usr/bin/env ruby
# Real stdio startup, a private HOME and synthetic browser files only.
require 'json'
require 'open3'
require 'fileutils'
require 'tmpdir'
require 'timeout'

server = File.realpath(ARGV.fetch(0) { abort 'usage: startup_profiles_e2e.rb SERVER EVIDENCE' })
evidence = File.expand_path(ARGV.fetch(1))
FileUtils.mkdir_p(evidence)

class StartupProbe
  attr_reader :home
  def initialize(server, evidence, name, delay: nil)
    @home = Dir.mktmpdir('km-profiles-', '/var/tmp')
    @log = File.open(File.join(evidence, "#{name}.jsonl"), 'w')
    @diagnostic = File.open(File.join(evidence, "#{name}.stderr"), 'w')
    @id = 0
    @session = nil
    env = {'HOME'=>@home, 'XDG_CONFIG_HOME'=>"#{@home}/.config", 'XDG_DATA_HOME'=>"#{@home}/.local/share",
      'XDG_CACHE_HOME'=>"#{@home}/.cache", 'XDG_STATE_HOME'=>"#{@home}/.local/state", 'KDEHOME'=>"#{@home}/.kde",
      'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}", 'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=/run/user/#{Process.uid}/bus",
      'KWIN_MCP_REPO'=>"#{@home}/no-repository", 'CARGO'=>'/usr/bin/false',
      'KWIN_MCP_TEST_STARTUP_DELAY_STAGE'=>delay ? 'after-bwrap' : nil,
      'KWIN_MCP_TEST_STARTUP_DELAY_MS'=>delay&.to_s}
    @input, @output, errors, @process = Open3.popen3(env, server, '--autoclean', '--width', '800', '--height', '600')
    @reader = Thread.new { IO.copy_stream(errors, @diagnostic) }
    rpc('initialize', protocolVersion:'2024-11-05', capabilities:{}, clientInfo:{name:'startup-profiles', version:'1'})
    @input.puts(JSON.generate(jsonrpc:'2.0', method:'notifications/initialized')); @input.flush
  end
  def rpc(method, params)
    @id += 1
    @input.puts(JSON.generate(jsonrpc:'2.0', id:@id, method:method, params:params)); @input.flush
    began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    result = Timeout.timeout(40) do
      loop do
        line = @output.gets or raise 'server EOF'
        reply = JSON.parse(line)
        break reply if reply['id'] == @id
      end
    end
    @log.puts(JSON.generate(method:method, params:params, elapsed:Process.clock_gettime(Process::CLOCK_MONOTONIC)-began, response:result)); @log.flush
    result
  end
  def call(name, arguments={})
    arguments = arguments.merge(session_id:@session) if @session && name != 'session_start'
    rpc('tools/call', name:name, arguments:arguments)
  end
  def data(reply)
    raise reply.inspect if reply['error'] || reply.dig('result','isError')
    reply.fetch('result').fetch('structuredContent')
  end
  def start(arguments={})
    reply = call('session_start', arguments)
    @session = reply.dig('result','structuredContent','session_id') || reply.dig('error','data','session_id')
    reply
  end
  def stop
    data(call('session_stop')) if @session
    @session = nil
  end
  def close
    stop
  ensure
    @input.close unless @input.closed?
    Timeout.timeout(30) { @process.value }
    @reader.join
    @diagnostic.close
    @log.close
    FileUtils.remove_entry(@home)
  end
end

probe = nil
begin
  probe = StartupProbe.new(server, evidence, 'fresh')
  profile = File.join(probe.home, '.config/google-chrome/Default')
  FileUtils.mkdir_p(profile)
  fifo = File.join(profile, 'Preferences')
  File.mkfifo(fifo)
  File.write(File.join(profile, 'host-only'), 'synthetic host marker')
  result = probe.data(probe.start(browser_profiles:'fresh'))
  raise result.inspect unless result['browser_profiles']=='fresh' && result['startup_timeout_seconds']==20
  workdir = result.fetch('workdir')
  probe.data(probe.call('launch_app', command:'test ! -e "$HOME/.config/google-chrome/Default/host-only" && test ! -e "$HOME/.config/google-chrome/Default/Preferences" && printf fresh > "$XDG_RUNTIME_DIR/profile-proof" && konsole --separate --nofork -e bash --noprofile --norc'))
  raise 'fresh profile mask failed' unless File.read(File.join(workdir,'profile-proof'))=='fresh'
  raise 'host profile changed' unless File.read(File.join(profile,'host-only'))=='synthetic host marker' && File.pipe?(fifo)
  probe.stop
  raise 'fresh mode persisted cookies' if File.exist?(File.join(probe.home,'.local/state/kwin-mcp/browser-cookies'))
  puts 'PASS fresh: terminal launched despite blocked host Preferences; host profile masked and unchanged; no cookie persistence'
ensure
  probe&.close
end

probe = nil
fifo = nil
begin
  probe = StartupProbe.new(server, evidence, 'blocked')
  profile = File.join(probe.home, '.config/google-chrome/Default')
  FileUtils.mkdir_p(profile)
  fifo = File.join(profile, 'Preferences')
  File.mkfifo(fifo)
  response = probe.start(startup_timeout_seconds:3)
  error = response.fetch('error')
  raise error.inspect unless error.dig('data','reason')=='hard_timeout' && error.dig('data','host_call_blocked')==true && error.dig('data','stage').include?("read Preferences #{fifo}")
  # Release only this fixture's blocked read, then verify deferred cleanup.
  File.open(fifo, File::RDWR | File::NONBLOCK) { |file| file.write('{"profile":{}}') }
  fifo = nil
  probe.stop
  raise 'deferred startup left disk state' unless Dir.glob(File.join(probe.home,'.cache/kwin-mcp/kwin-mcp-*')).empty?
  puts 'PASS blocked: timeout identifies exact Preferences operation/path; stop succeeds after release and removes disk state'
ensure
  File.open(fifo, File::RDWR | File::NONBLOCK) { |file| file.write('{}') } if fifo
  probe&.close
end

probe = nil
begin
  probe = StartupProbe.new(server, evidence, 'timeout-validation', delay:3000)
  [0,301].each do |seconds|
    response = probe.start(startup_timeout_seconds:seconds)
    raise response.inspect unless response.dig('error','code') == -32602
  end
  response = probe.start(browser_profiles:'fresh', startup_timeout_seconds:2)
  raise response.inspect unless response.dig('error','data','reason')=='hard_timeout'
  probe.stop
  result = probe.data(probe.start(browser_profiles:'fresh', startup_timeout_seconds:10))
  raise result.inspect unless result['startup_timeout_seconds']==10
  probe.data(probe.call('window_list'))
  probe.stop
  puts 'PASS budget: invalid bounds rejected; 2s cancels a delayed real startup; 10s completes it'
ensure
  probe&.close
end
