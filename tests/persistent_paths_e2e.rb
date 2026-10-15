#!/usr/bin/env ruby
# Real stdio sessions and direct filesystem checks, with a fresh private HOME.
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'timeout'

binary, evidence = ARGV
abort 'usage: persistent_paths_e2e.rb /path/to/kwin-mcp /private/evidence-directory' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence, mode: 0o700)
home = File.join(evidence, 'home')
FileUtils.mkdir_p(home, mode: 0o700)
%w[.config .local/share .local/state .cache .kde].each { |p| FileUtils.mkdir_p(File.join(home, p)) }
profile = File.join(home, '.local/share/gateway-chrome')
other = File.join(home, ".local/share/other app's data")
[profile, other].each { |p| FileUtils.mkdir_p(p, mode: 0o700) }
File.write(File.join(profile, 'owned-marker'), 'generation-0')
profile_identity = [File.stat(profile).dev, File.stat(profile).ino]
File.write(File.join(home, 'plain-file'), 'plain-host')
File.write(File.join(home, 'not-a-directory'), 'fixture')
FileUtils.mkdir_p(File.join(home, 'public-directory'), mode: 0o755)
FileUtils.mkdir_p(File.join(home, 'unsafe-parent/child'), mode: 0o700)
File.chmod(0o777, File.join(home, 'unsafe-parent'))
File.symlink(profile, File.join(home, 'linked-profile'))
File.symlink(File.dirname(profile), File.join(home, 'linked-parent'))

# Deny host wallet and Secret Service access. Scope creation still uses the
# actual logged-in systemd user manager through this filtered bus.
runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.euid}")
proxy_socket = File.join(evidence, 'filtered-bus')
proxy_pid = Process.spawn('xdg-dbus-proxy', "unix:path=#{runtime}/bus", proxy_socket,
                          '--filter', '--talk=org.freedesktop.systemd1',
                          out: File::NULL, err: File.join(evidence, 'proxy.stderr'))
Timeout.timeout(10) { sleep 0.05 until File.socket?(proxy_socket) }
environment = {
  'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
  'XDG_DATA_HOME' => File.join(home, '.local/share'),
  'XDG_STATE_HOME' => File.join(home, '.local/state'),
  'XDG_CACHE_HOME' => File.join(home, '.cache'),
  'KDEHOME' => File.join(home, '.kde'), 'XAUTHORITY' => nil,
  'XDG_RUNTIME_DIR' => runtime, 'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{proxy_socket}",
  'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
  'KWIN_MCP_REPO' => File.join(evidence, 'no-source-reload'), 'CARGO' => '/usr/bin/false'
}

class PersistentProof
  def initialize(binary, environment, evidence, generation)
    @input, @output, errors, @process = Open3.popen3(environment, binary, '--autoclean', '--width', '800', '--height', '600')
    @diagnostic = File.open(File.join(evidence, "server-#{generation}.stderr"), 'w')
    @diagnostic.sync = true
    @reader = Thread.new { IO.copy_stream(errors, @diagnostic) }
    @serial = 0
    rpc('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'persistent-paths-proof', version: '1'}})
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); @input.flush
  end

  def rpc(method, params)
    @serial += 1
    @input.puts(JSON.generate(jsonrpc: '2.0', id: @serial, method: method, params: params)); @input.flush
    Timeout.timeout(75) do
      loop do
        message = JSON.parse(@output.gets || raise('server EOF'))
        return message if message['id'] == @serial
      end
    end
  end

  def call(name, arguments = {}, error: false)
    response = rpc('tools/call', {name: name, arguments: arguments})
    failed = response['error'] || response.dig('result', 'isError')
    if error
      raise "invalid persistent_paths accepted: #{arguments.inspect}" unless failed
      message = response.dig('error', 'message') || response.dig('result', 'content').to_s
      raise "missing persistent_paths diagnostic: #{response.inspect}" unless message.include?('persistent_paths')
      return
    end
    raise response.inspect if failed
    response.fetch('result').fetch('structuredContent', {})
  end

  def close
    @input.close
    status = Timeout.timeout(20) { @process.value }
    @reader.join
    @diagnostic.close
    raise "server exit: #{status}" unless status.success?
  end

  def wait_for_diagnostic(text)
    Timeout.timeout(10) { sleep 0.05 until File.read(@diagnostic.path).include?(text) }
  end
end

results = []
proof = nil
begin
  proof = PersistentProof.new(binary, environment, evidence, 1)
  invalid = {
    relative: ['relative'], parent: [File.join(home, '../escape')],
    home: [home], outside: ['/var/lib'], missing: [File.join(home, 'missing')],
    file: [File.join(home, 'not-a-directory')], public: [File.join(home, 'public-directory')],
    unsafe_ancestor: [File.join(home, 'unsafe-parent/child')],
    symlink: [File.join(home, 'linked-profile')],
    symlink_ancestor: [File.join(home, 'linked-parent/gateway-chrome')],
    duplicate: [profile, profile], overlap: [profile, File.join(profile, 'nested')],
    browser: [File.join(home, '.config/google-chrome')],
    storage: [File.join(home, '.cache/kwin-mcp')],
    cookie_state: [File.join(home, '.local/state/kwin-mcp/browser-cookies')]
  }
  invalid.each do |name, paths|
    proof.call('session_start', {persistent_paths: paths}, error: true)
    raise 'failed start left a session' unless proof.call('session_list').fetch('sessions').empty?
    results << "reject_#{name}=PASS"
  end
  browser_alias = File.join(home, '.config/google-chrome')
  File.symlink(profile, browser_alias)
  proof.call('session_start', {persistent_paths: [profile]}, error: true)
  File.unlink(browser_alias)
  results << 'reject_browser_alias=PASS'
  proof.call('session_start', {persistent_paths: [profile], writable_paths: [profile]}, error: true)
  results << 'reject_private_copy_overlap=PASS'
  3.times do |cycle|
    if cycle == 2
      proof.close
      proof = PersistentProof.new(binary, environment, evidence, 2)
    end
    started = proof.call('session_start', {persistent_paths: [profile, other]})
    id = started.fetch('session_id')
    raise 'mounts not reported' unless started.fetch('persistent_paths') == [profile, other]
    raise 'unexpected cookie processing error' unless started.fetch('cookie_errors').empty?
    record = proof.call('session_list').fetch('sessions').find { |s| s['session_id'] == id }
    marker = File.join(profile, 'owned-marker')
    # Only fixture data is read here; no browser, authentication or cookie jars.
    command = "test \"$(cat #{Shellwords.escape(marker)})\" = generation-#{cycle} && " \
              "test \"$(stat -c %d:%i #{Shellwords.escape(profile)})\" = #{profile_identity.join(':')} && " \
              "test \"$(cat #{Shellwords.escape(File.join(home, 'plain-file'))})\" = plain-host && " \
              "printf generation-#{cycle + 1} > #{Shellwords.escape(marker)} && " \
              "printf disposable > #{Shellwords.escape(File.join(home, 'plain-file'))} && " \
              "printf second-mount > #{Shellwords.escape(File.join(other, 'marker'))}"
    proof.call('launch_app', {session_id: id, command: command})
    raise 'session did not write through persistent mount' unless File.read(marker) == "generation-#{cycle + 1}"
    raise 'second mount not writable' unless File.read(File.join(other, 'marker')) == 'second-mount'
    raise 'ordinary write escaped overlay' unless File.read(File.join(home, 'plain-file')) == 'plain-host'
    raise 'persistent directory inode changed' unless [File.stat(profile).dev, File.stat(profile).ino] == profile_identity
    # Neither staging nor overlay storage may contain profile contents.
    disk = File.join(home, '.cache/kwin-mcp', File.basename(record.fetch('workdir')))
    copies = Dir.glob(File.join(disk, '**', 'owned-marker'), File::FNM_DOTMATCH)
    raise "persistent contents copied into session storage: #{copies}" unless copies.empty?
    stopped = proof.call('session_stop', {session_id: id})
    raise 'unexpected cookie save error' unless stopped.fetch('cookie_errors').empty?
    raise 'workdir survived autoclean' if File.exist?(record.fetch('workdir')) || File.exist?(disk)
    raise 'persistent contents removed on stop' unless File.read(marker) == "generation-#{cycle + 1}"
    results << "replacement_cycle_#{cycle + 1}=PASS"
  end
  proof.close
  proof = nil
  # The debug server's existing host-scan delay lets us replace the pathname
  # after validation. A reopened replacement must never become writable.
  proof = PersistentProof.new(binary, environment.merge('KWIN_MCP_TEST_BLOCK_HOST_SCAN_MS' => '3000'), evidence, 'race')
  original = File.join(home, '.local/share/pinned-original')
  request = Thread.new { proof.rpc('tools/call', {name: 'session_start', arguments: {persistent_paths: [profile]}}) }
  begin
    proof.wait_for_diagnostic('test host scan blocked')
    File.rename(profile, original)
    FileUtils.mkdir_p(profile, mode: 0o700)
    File.write(File.join(profile, 'owned-marker'), 'replacement-must-not-mount')
    response = request.value
    raise 'replaced source directory was mounted' unless response['error'] || response.dig('result', 'isError')
    raise 'failed race left a session' unless proof.call('session_list').fetch('sessions').empty?
    raise 'original directory was changed' unless File.read(File.join(original, 'owned-marker')) == 'generation-3'
    raise 'replacement directory was changed' unless File.read(File.join(profile, 'owned-marker')) == 'replacement-must-not-mount'
  ensure
    if File.directory?(original)
      FileUtils.rm_rf(profile)
      File.rename(original, profile)
    end
  end
  results << 'reject_source_replacement_after_validation=PASS'
  proof.close
  proof = nil
  # Omitted persistent_paths must preserve the old disposable behavior.
  proof = PersistentProof.new(binary, environment, evidence, 3)
  started = proof.call('session_start')
  id = started.fetch('session_id')
  raise 'default acquired persistent mounts' unless started.fetch('persistent_paths').empty?
  proof.call('launch_app', {session_id: id, command: "printf private-copy > #{Shellwords.escape(File.join(profile, 'owned-marker'))}"})
  raise 'default session wrote to host directory' unless File.read(File.join(profile, 'owned-marker')) == 'generation-3'
  proof.call('session_stop', {session_id: id})
  saved = Dir.glob(File.join(home, '.local/state/kwin-mcp/browser-cookies', '**', '*'), File::FNM_DOTMATCH).select { |p| File.file?(p) }
  raise 'dedicated profile generated saved cookie files' unless saved.empty?
  results << 'default_disposable_and_no_cookie_copies=PASS'
  File.write(File.join(evidence, 'results.txt'), results.join("\n") + "\n")
  puts results
ensure
  begin
    proof&.close
  ensure
    Process.kill('TERM', proxy_pid)
    Process.wait(proxy_pid)
  end
end
