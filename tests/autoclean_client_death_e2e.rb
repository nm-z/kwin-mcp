#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'timeout'
require 'tmpdir'

# A real filesystem read holds session_start after the disk overlay exists,
# before a Session (and its teardown) can take ownership. No desktop is started.
binary, evidence, mode = ARGV
abort 'usage: autoclean_client_death_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
home = File.join(evidence, 'home')
fifo = File.join(home, '.config/kdeglobals')

if mode == 'controller'
  env = {
    'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
    'XDG_DATA_HOME' => File.join(home, '.local/share'),
    'XDG_CACHE_HOME' => File.join(home, '.cache'),
    'XDG_STATE_HOME' => File.join(home, '.local/state'),
    'KDEHOME' => File.join(home, '.kde'),
    'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
    'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{Process.euid}/bus",
    'KWIN_MCP_REPO' => File.join(evidence, 'no-repository'),
  }
  input, output, error, server = Open3.popen3(env, binary, '--autoclean')
  Thread.new { File.open(File.join(evidence, 'server.stderr'), 'w') { |log| IO.copy_stream(error, log) } }
  input.puts(JSON.generate(jsonrpc: '2.0', id: 1, method: 'initialize', params: {
    protocolVersion: '2025-06-18', capabilities: {},
    clientInfo: {name: 'autoclean-client-death-proof', version: '1'},
  }))
  input.flush
  Timeout.timeout(10) do
    loop do
      reply = JSON.parse(output.gets || raise('server EOF during initialize'))
      next unless reply['id'] == 1
      raise reply.inspect unless reply['result']
      break
    end
  end
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
  input.puts(JSON.generate(jsonrpc: '2.0', id: 2, method: 'tools/call', params: {name: 'session_start', arguments: {}}))
  input.flush
  # Preserve stdin after the controller dies; its pidfd must initiate shutdown.
  keeper = Process.spawn('sleep', '120', 3 => input, close_others: true, out: File::NULL, err: File::NULL)
  File.write(File.join(evidence, 'ready.pending'), JSON.generate(server: server.pid, keeper: keeper))
  File.rename(File.join(evidence, 'ready.pending'), File.join(evidence, 'ready.json'))
  sleep 120
  exit
end

controller = record = writer = nil
begin
  %w[.config .cache .local/share .local/state .kde].each { |name| FileUtils.mkdir_p(File.join(home, name)) }
  File.mkfifo(fifo, 0o600)
  controller = Process.spawn(RbConfig.ruby, __FILE__, binary, evidence, 'controller',
                             out: File.join(evidence, 'controller.stdout'), err: File.join(evidence, 'controller.stderr'))
  Timeout.timeout(15) { sleep 0.05 until File.exist?(File.join(evidence, 'ready.json')) }
  record = JSON.parse(File.read(File.join(evidence, 'ready.json')))
  workdir = File.join(Dir.tmpdir, "kwin-mcp-#{record.fetch('server')}-1")
  disk = File.join(home, '.cache/kwin-mcp', File.basename(workdir))
  # Opening the writer succeeds only once production prepare_host_view opens
  # kdeglobals. Hold it open so read_to_string cannot complete before client death.
  Timeout.timeout(15) do
    loop do
      begin
        writer = File.open(fifo, File::WRONLY | File::NONBLOCK)
        break
      rescue Errno::ENXIO
        sleep 0.05
      end
    end
  end
  raise 'disk overlay was not created' unless File.directory?(File.join(disk, 'overlay-upper'))
  raise 'startup workdir was not created' unless File.directory?(workdir)
  # Exercise permission repair and prove symlinks cannot redirect deletion.
  sentinel = File.join(evidence, 'outside')
  FileUtils.mkdir_p(sentinel)
  File.write(File.join(sentinel, 'keep'), 'outside session storage')
  File.chmod(0o500, sentinel)
  File.symlink(sentinel, File.join(disk, 'outside-link'))
  FileUtils.mkdir_p(File.join(disk, 'mode-zero', 'nested'))
  File.chmod(0, File.join(disk, 'mode-zero'))
  Process.kill('KILL', controller)
  Process.wait(controller)
  controller = nil
  Timeout.timeout(45) do
    loop do
      stat = "/proc/#{record.fetch('server')}/stat"
      break if !File.exist?(stat) || File.read(stat).split[2] == 'Z'
      sleep 0.05
    end
  end
  observed = {
    workdir_present: File.exist?(workdir), disk_present: File.exist?(disk),
    inherited_stdin_alive: File.directory?("/proc/#{record.fetch('keeper')}"),
    outside_unchanged: File.read(File.join(sentinel, 'keep')) == 'outside session storage',
    outside_mode: File.stat(sentinel).mode & 0o777,
  }
  File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(observed.merge(record)))
  raise observed.inspect unless observed == {
    workdir_present: false, disk_present: false, inherited_stdin_alive: true,
    outside_unchanged: true, outside_mode: 0o500,
  }
  puts 'autoclean_client_death=PASS (unpublished startup; tmp and disk removed; inherited stdin and outside directory preserved)'
ensure
  writer&.close
  if controller
    Process.kill('KILL', controller) rescue Errno::ESRCH
    Process.wait(controller) rescue Errno::ECHILD
  end
  if record
    Process.kill('TERM', record.fetch('keeper')) rescue Errno::ESRCH
    stat = "/proc/#{record.fetch('server')}/stat"
    if File.exist?(stat) && File.read(stat).split[2] != 'Z'
      Process.kill('TERM', record.fetch('server')) rescue Errno::ESRCH
    end
  end
end
