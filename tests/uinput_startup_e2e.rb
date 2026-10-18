#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'timeout'

# Mount only inside a private namespace, then restore the caller's original
# credentials before execing the production server. No host chmod or udev edits.
if ARGV.first == '--namespace'
  _, kind, directory, binary, uid, gid, groups = ARGV
  abort 'namespace helper must run as root' unless Process.euid.zero?
  fixture = File.join(directory, 'fixture')
  target = case kind
           when 'missing' then '/dev'
           when 'denied', 'ioctl' then '/dev/uinput'
           when 'sysfs' then '/sys/devices/virtual/input'
           else abort "unknown case #{kind}"
           end
  abort "bind mount failed: #{target}" unless system('mount', '--bind', fixture, target)
  Process.groups = groups.split(',').map { |value| Integer(value) }
  Process::GID.change_privilege(Integer(gid))
  Process::UID.change_privilege(Integer(uid))
  home = File.join(directory, 'home')
  ENV.update(
    'HOME' => home,
    'XDG_CONFIG_HOME' => File.join(home, '.config'),
    'XDG_DATA_HOME' => File.join(home, '.local/share'),
    'XDG_CACHE_HOME' => File.join(home, '.cache'),
    'XDG_STATE_HOME' => File.join(home, '.local/state'),
    'KDEHOME' => File.join(home, '.kde'),
    'XDG_RUNTIME_DIR' => "/run/user/#{uid}",
    'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{uid}/bus",
    'KWIN_MCP_REPO' => File.join(directory, 'no-repository'),
    'CARGO' => '/usr/bin/false'
  )
  %w[DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET XAUTHORITY].each { |name| ENV.delete(name) }
  File.write(File.join(directory, 'context.json'), JSON.pretty_generate({
    pid: Process.pid,
    status: File.read('/proc/self/status').lines.select { |line| line.match?(/\A(?:Uid|Gid|Groups):/) }.map(&:strip),
    user_namespace: File.readlink('/proc/self/ns/user'),
    mount_namespace: File.readlink('/proc/self/ns/mnt')
  }))
  exec binary, '--autoclean'
end

binary, evidence = ARGV
abort 'usage: uinput_startup_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
abort 'run as a non-root user with passwordless sudo, /dev/uinput, and a user runtime/bus' if Process.euid.zero?
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
cases = {
  'denied' => ['open read/write /dev/uinput', 'os error 13'],
  'missing' => ['open read/write /dev/uinput', 'os error 2'],
  'ioctl' => ['configure key capabilities on /dev/uinput', 'os error 25'],
  'sysfs' => ['read evdev directory /sys/devices/virtual/input/input', 'os error 2']
}
cases.each do |kind, expected|
  directory = File.join(evidence, kind)
  FileUtils.mkdir_p(directory)
  home = File.join(directory, 'home')
  %w[.config .local/share .cache .local/state .kde].each { |name| FileUtils.mkdir_p(File.join(home, name)) }
  fixture = File.join(directory, 'fixture')
  if %w[missing sysfs].include?(kind)
    FileUtils.mkdir_p(fixture)
  else
    File.write(fixture, '')
    File.chmod(kind == 'denied' ? 0o000 : 0o600, fixture)
  end
  command = ['sudo', '-n', 'unshare', '--mount', '--propagation', 'private', '--',
             RbConfig.ruby, File.realpath(__FILE__), '--namespace', kind, directory,
             binary, Process.uid.to_s, Process.gid.to_s, Process.groups.join(',')]
  File.open(File.join(directory, 'rpc.jsonl'), 'w') do |trace|
    File.open(File.join(directory, 'server.stderr'), 'w') do |diagnostic|
      Open3.popen3(*command) do |input, output, error, process|
        stderr_reader = Thread.new { IO.copy_stream(error, diagnostic) }
        id = 0
        rpc = lambda do |method, params|
          id += 1
          request = {jsonrpc: '2.0', id: id, method: method, params: params}
          trace.puts(JSON.generate({request: request})); trace.flush
          input.puts(JSON.generate(request)); input.flush
          Timeout.timeout(40) do
            loop do
              line = output.gets
              raise "server EOF (see #{directory}/server.stderr)" unless line
              response = JSON.parse(line)
              trace.puts(JSON.generate({response: response})); trace.flush
              break response if response['id'] == id
            end
          end
        end
        begin
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'uinput-startup-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush
          response = rpc.call('tools/call', {name: 'session_start', arguments: {}})
          message = response.dig('error', 'message')
          raise "expected failed session_start: #{response.inspect}" unless message
          context = JSON.parse(File.read(File.join(directory, 'context.json')))
          fragments = ['uinput: mouse:', *expected, *context.fetch('status'),
                       "user_namespace=#{context.fetch('user_namespace')}",
                       "mount_namespace=#{context.fetch('mount_namespace')}"]
          fragments.each { |fragment| raise "missing #{fragment.inspect}: #{message}" unless message.include?(fragment) }
          sessions = rpc.call('tools/call', {name: 'session_list', arguments: {}})
          raise "failed startup left a session: #{sessions.inspect}" unless sessions.dig('result', 'structuredContent', 'sessions') == []
          raise 'failed startup left a workdir' unless Dir.glob("/tmp/kwin-mcp-#{context.fetch('pid')}-*").empty?
          puts "#{kind}: PASS — #{message}"
        ensure
          input.close
          status = Timeout.timeout(15) { process.value }
          stderr_reader.join
          raise "server exit #{status.inspect}" unless status.success?
        end
      end
    end
  end
ensure
  FileUtils.remove_entry(home) if home && File.exist?(home)
  FileUtils.remove_entry(fixture) if fixture && File.exist?(fixture)
end
