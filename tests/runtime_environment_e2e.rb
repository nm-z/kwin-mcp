#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'shellwords'
require 'socket'
require 'timeout'
require 'tmpdir'

desktop_variables = %w[DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET]
if desktop_variables.any? { |name| ENV[name] && !ENV[name].empty? }
  exec(desktop_variables.to_h { |name| [name, nil] }, RbConfig.ruby, __FILE__, *ARGV)
end
cgroup = File.read('/proc/self/cgroup')
abort 'run this proof from a worker/service outside a graphical app.scope or session.slice' if cgroup.include?('/session.slice/') || (cgroup.include?('/app.slice/') && cgroup.include?('.scope'))

# Run the compiled server on the KDE host, never cargo or an internal mock.
binary, evidence = ARGV
abort 'usage: runtime_environment_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)

class RuntimeProof
  def initialize(binary, directory, overrides)
    @binary, @directory = binary, directory
    # Socket addresses must fit sockaddr_un independently of artifact paths.
    @fixture_root = Dir.mktmpdir('kmr-', '/var/tmp')
    private_home = File.join(@fixture_root, 'home')
    @private_home = private_home
    %w[.config .local/share .cache .local/state .kde].each do |name|
      FileUtils.mkdir_p(File.join(private_home, name))
    end
    @environment = {
      'HOME' => private_home,
      'XDG_CONFIG_HOME' => File.join(private_home, '.config'),
      'XDG_DATA_HOME' => File.join(private_home, '.local/share'),
      'XDG_CACHE_HOME' => File.join(private_home, '.cache'),
      'XDG_STATE_HOME' => File.join(private_home, '.local/state'),
      'KDEHOME' => File.join(private_home, '.kde'),
      'XDG_RUNTIME_DIR' => nil,
      'DBUS_SESSION_BUS_ADDRESS' => nil,
      'WAYLAND_DISPLAY' => nil,
      'WAYLAND_SOCKET' => nil,
      'DISPLAY' => nil,
      'XAUTHORITY' => nil,
      'KWIN_MCP_REPO' => File.join(directory, 'no-repository'),
      'CARGO' => '/usr/bin/false',
    }.merge(overrides)
  end

  def rpc(method, params)
    @id += 1
    request = {jsonrpc: '2.0', id: @id, method: method, params: params}
    @trace.puts(JSON.generate({request: request})); @trace.flush
    @input.puts(JSON.generate(request)); @input.flush
    Timeout.timeout(60) do
      loop do
        line = @output.gets
        raise 'unexpected server EOF' unless line
        message = JSON.parse(line)
        @trace.puts(JSON.generate({response: message})); @trace.flush
        return message if message['id'] == @id
      end
    end
  end

  def call(name, arguments = {})
    rpc('tools/call', {name: name, arguments: arguments})
  end

  def result(response)
    raise response.inspect if response['error'] || response.dig('result', 'isError')
    response.fetch('result')
  end

  def run(expected_error)
    prepare_socket_proof unless expected_error
    File.open(File.join(@directory, 'rpc.jsonl'), 'w') do |trace|
      @trace = trace
      File.open(File.join(@directory, 'server.stderr'), 'w') do |diagnostic|
        Open3.popen3(@environment, @binary, '--autoclean', '--width', '1232', '--height', '924') do |input, output, error, process|
          @input, @output, @id = input, output, 0
          stderr_reader = Thread.new { IO.copy_stream(error, diagnostic) }
          session_id = nil
          begin
            result(rpc('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'runtime-environment-proof', version: '1'}}))
            input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush
            response = call('session_start')
            session_id = response.dig('result', 'structuredContent', 'session_id') || response.dig('error', 'data', 'session_id')
            if expected_error
              message = response.dig('error', 'message') || response.dig('result', 'content')&.map { |item| item['text'] }.join(' ')
              raise "expected actionable error #{expected_error.inspect}, got #{response.inspect}" unless (response['error'] || response.dig('result', 'isError')) && message&.include?(expected_error)
              sessions = result(call('session_list')).dig('structuredContent', 'sessions')
              raise 'invalid environment created a session' unless sessions == []
              raise 'invalid environment created a sandbox workdir' unless Dir.glob("/tmp/kwin-mcp-#{process.pid}-*").empty?
              session_id = nil
            else
              started = result(response)
              session_id = started.dig('structuredContent', 'session_id')
              raise 'session_start omitted session_id' unless session_id
              sessions = result(call('session_list')).dig('structuredContent', 'sessions')
              record = sessions.find { |session| session['session_id'] == session_id }
              raise 'CLI dimensions not applied' unless record && record['width'] == 1232 && record['height'] == 924
              raise 'session workdir not present' unless File.directory?(record.fetch('workdir'))
              sandbox_pid = record.fetch('sandbox_pid')
              raise 'sandbox process not present' unless File.directory?("/proc/#{sandbox_pid}")
              File.write(File.join(@directory, 'system-proof.json'), JSON.pretty_generate(record))
              result(call('window_list', {session_id: session_id}))
              initial_inode = File.stat(File.join(@private_home, 'nongraphical.sock')).ino
              [1, 2].each do |generation|
                if generation == 2
                  previous_listener = @nongraphical_socket
                  previous_thread = @nongraphical_thread
                  replacement_path = File.join(@private_home, 'nongraphical.sock.next')
                  start_nongraphical_listener(2, replacement_path)
                  previous_listener.close
                  previous_thread.join(2)
                  File.rename(replacement_path, File.join(@private_home, 'nongraphical.sock'))
                  replacement_inode = File.stat(File.join(@private_home, 'nongraphical.sock')).ino
                  raise 'daemon replacement did not create a new inode' if replacement_inode == initial_inode
                  File.write(File.join(@directory, 'replacement-inodes.json'), JSON.pretty_generate({initial: initial_inode, replacement: replacement_inode}))
                end
                command = [RbConfig.ruby, File.join(@private_home, 'socket-proof.rb'), generation.to_s].map { |argument| Shellwords.escape(argument) }.join(' ')
                result(call('launch_app', {session_id: session_id, command: command}))
                name = "socket-proof-#{generation}.json"
                artifact = File.join(@directory, name)
                result(call('export_file', {session_id: session_id, session_path: File.join(@private_home, name), host_path: artifact}))
                proof = JSON.parse(File.read(artifact)).fetch('checks')
                raise "socket isolation/replacement failed: #{proof.inspect}" unless proof.values.all? { |value| value == true }
              end
              result(call('launch_app', {session_id: session_id, command: "kdialog --title 'Runtime fallback proof' --msgbox 'Session startup and capture work without inherited desktop variables.'"}))
              capture = result(call('screenshot', {session_id: session_id})).fetch('structuredContent')
              raise 'captured the wrong window' unless capture.dig('window', 'title').include?('Runtime fallback proof')
              FileUtils.cp(capture.fetch('path'), File.join(@directory, 'runtime-fallback.png'))
              File.write(File.join(@directory, 'capture.json'), JSON.pretty_generate(capture))
              result(call('session_stop', {session_id: session_id}))
              session_id = nil
              raise 'stopped sandbox still live' if File.directory?("/proc/#{sandbox_pid}")
              raise 'stopped workdir still exists' if File.exist?(record.fetch('workdir'))
              raise 'stopped session still listed' unless result(call('session_list')).dig('structuredContent', 'sessions') == []
            end
          ensure
            begin
              result(call('session_stop', {session_id: session_id})) if session_id
            ensure
              input.close
              status = Timeout.timeout(15) { process.value }
              stderr_reader.join
              raise "server exit #{status.exitstatus}" unless status.success?
            end
          end
        end
      end
    end
  ensure
    begin
      @nongraphical_socket&.close
      @nongraphical_thread&.join(2)
      @hidden_socket&.close
      if @graphical_pid
        Process.kill('TERM', @graphical_pid)
        Timeout.timeout(5) { Process.wait(@graphical_pid) }
      end
    ensure
      FileUtils.remove_entry(@fixture_root) if @fixture_root && File.directory?(@fixture_root)
    end
  end

  def prepare_socket_proof
    nongraphical = File.join(@private_home, 'nongraphical.sock')
    graphical = File.join(@private_home, 'graphical.sock')
    start_nongraphical_listener(1)
    # An allowed socket whose parent is itself a hidden root must not expose
    # that parent's sibling files through the internal alias directory.
    hidden_root = File.join(@private_home, '.cache', 'kwin-mcp')
    FileUtils.mkdir_p(hidden_root)
    File.write(File.join(hidden_root, 'runtime-proof-private-marker'), 'private fixture')
    @hidden_socket = UNIXServer.new(File.join(hidden_root, 'runtime-proof-control.sock'))
    reader, writer = IO.pipe
    child = 'server = UNIXServer.new(ARGV.fetch(0)); ready = IO.new(3); ready.puts("ready"); ready.close; loop { peer = server.accept; peer.write("graphical-proof"); peer.close }'
    @graphical_pid = Process.spawn({'WAYLAND_DISPLAY' => 'runtime-proof-host-wayland'}, RbConfig.ruby, '-rsocket', '-e', child, graphical, 3 => writer, out: File::NULL, err: File.join(@directory, 'graphical.stderr'))
    writer.close
    raise 'graphical fixture did not start' unless Timeout.timeout(10) { reader.gets } == "ready\n"
    reader.close
    host_runtime = "/run/user/#{Process.euid}"
    File.symlink(host_runtime, File.join(@private_home, 'host-runtime-link'))
    secrets = [File.join(host_runtime, 'bus')] + %w[keyring p11-kit].flat_map { |name| Dir.glob(File.join(host_runtime, name, '**', '*')).select { |path| File.socket?(path) } }
    identity = lambda { |path| metadata = File.stat(path); [metadata.dev, metadata.ino] }
    manifest = {nongraphical: nongraphical, graphical: graphical, graphical_identity: identity.call(graphical), secrets: secrets.map { |path| {path: path, identity: identity.call(path)} }}
    File.write(File.join(@private_home, 'socket-manifest.json'), JSON.generate(manifest))
    File.write(File.join(@directory, 'host-socket-manifest.json'), JSON.pretty_generate(manifest))
    File.write(File.join(@private_home, 'socket-proof.rb'), <<~'RUBY')
      require 'json'
      require 'socket'
      require 'timeout'
      generation = Integer(ARGV.fetch(0))
      manifest = JSON.parse(File.read(File.join(ENV.fetch('HOME'), 'socket-manifest.json')))
      identity = lambda do |path|
        metadata = File.stat(path)
        [metadata.dev, metadata.ino]
      rescue SystemCallError
        nil
      end
      connect = lambda do |path|
        Timeout.timeout(2) do
          UNIXSocket.open(path) { |peer| peer.read }
        end
      rescue SystemCallError, Timeout::Error
        nil
      end
      root = '/run/kwin-mcp-host-sockets'
      mounts = Dir.children(root).map { |name| File.join(root, name) }
      secrets = manifest.fetch('secrets')
      runtime_relative = lambda { |path| path.delete_prefix("/run/user/#{Process.euid}/") }
      daemon_response = connect.call(manifest.fetch('nongraphical'))
      proof = {
        'nongraphical_socket_reachable' => daemon_response == "nongraphical-proof-#{generation}",
        'graphical_socket_blocked' => connect.call(manifest.fetch('graphical')).nil?,
        'graphical_socket_aliases_masked' => mounts.all? { |mount| connect.call(File.join(mount, File.basename(manifest.fetch('graphical')))).nil? },
        'host_secret_socket_aliases_masked' => mounts.all? do |mount|
          secrets.all? do |secret|
            relative = runtime_relative.call(secret.fetch('path'))
            identity.call(File.join(mount, relative)) != secret.fetch('identity') &&
              identity.call(File.join(mount, 'host-runtime-link', relative)) != secret.fetch('identity')
          end
        end,
        'startup_hidden_roots_masked' => mounts.all? do |mount|
          hidden = File.join(mount, '.cache', 'kwin-mcp')
          (!File.directory?(hidden) || Dir.empty?(hidden)) &&
            !File.exist?(File.join(mount, 'runtime-proof-private-marker'))
        end,
        'host_secret_paths_hidden' => secrets.all? { |secret| identity.call(secret.fetch('path')) != secret.fetch('identity') },
        'private_runtime_secrets_isolated' => secrets.all? do |secret|
          relative = runtime_relative.call(secret.fetch('path'))
          identity.call(File.join(ENV.fetch('XDG_RUNTIME_DIR'), relative)) != secret.fetch('identity')
        end,
      }
      File.write(File.join(ENV.fetch('HOME'), "socket-proof-#{generation}.json"), JSON.pretty_generate({generation: generation, daemon_response: daemon_response, checks: proof}))
      exit(proof.values.all? ? 0 : 1)
    RUBY
  end

  def start_nongraphical_listener(generation, path = File.join(@private_home, 'nongraphical.sock'))
    listener = UNIXServer.new(path)
    listener.close_on_exec = true
    @nongraphical_socket = listener
    @nongraphical_thread = Thread.new do
      begin
        loop do
          connection = listener.accept
          connection.write("nongraphical-proof-#{generation}")
          connection.close
        end
      rescue IOError, Errno::EBADF
        # Closing only our fixture listener ends this generation's accept loop.
      end
    end
  end
end

fixtures = File.join(evidence, 'input-fixtures')
FileUtils.mkdir_p(fixtures)
public_runtime = File.join(fixtures, 'public-runtime')
private_runtime = File.join(fixtures, 'private-runtime-without-bus')
runtime_file = File.join(fixtures, 'runtime-file')
FileUtils.mkdir_p(public_runtime)
File.chmod(0o755, public_runtime)
FileUtils.mkdir_p(private_runtime)
File.chmod(0o700, private_runtime)
File.write(runtime_file, 'not a runtime directory')
cases = [
  ['missing-environment', {}, nil],
  ['relative-runtime', {'XDG_RUNTIME_DIR' => 'relative'}, 'XDG_RUNTIME_DIR must be an absolute'],
  ['empty-runtime', {'XDG_RUNTIME_DIR' => ''}, 'XDG_RUNTIME_DIR must be an absolute'],
  ['missing-runtime', {'XDG_RUNTIME_DIR' => File.join(evidence, 'does-not-exist')}, 'host runtime directory'],
  ['runtime-file', {'XDG_RUNTIME_DIR' => runtime_file}, 'is not a directory'],
  ['public-runtime', {'XDG_RUNTIME_DIR' => public_runtime}, 'must have mode 0700'],
  ['wrong-owner-runtime', {'XDG_RUNTIME_DIR' => '/run'}, 'owned by UID 0'],
  ['missing-bus', {'XDG_RUNTIME_DIR' => private_runtime}, 'DBUS_SESSION_BUS_ADDRESS is unset'],
  ['empty-bus', {'DBUS_SESSION_BUS_ADDRESS' => ''}, 'DBUS_SESSION_BUS_ADDRESS is empty'],
  ['invalid-bus', {'DBUS_SESSION_BUS_ADDRESS' => 'not-a-dbus-address'}, 'invalid DBUS_SESSION_BUS_ADDRESS'],
  ['missing-bus-override', {'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{evidence}/no-bus"}, 'DBUS_SESSION_BUS_ADDRESS socket'],
  ['file-bus-override', {'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{runtime_file}"}, 'is not a Unix socket'],
  ['wrong-owner-bus-override', {'DBUS_SESSION_BUS_ADDRESS' => 'unix:path=/run/dbus/system_bus_socket'}, 'not owned by the runtime directory'],
]
abort 'run as a logged-in non-root desktop user' if Process.euid.zero?
File.open(File.join(evidence, 'results.txt'), 'w') do |results|
  cases.each do |name, overrides, expected_error|
    directory = File.join(evidence, name)
    FileUtils.mkdir_p(directory)
    RuntimeProof.new(binary, directory, overrides).run(expected_error)
    if name == 'missing-environment'
      stderr = File.read(File.join(directory, 'server.stderr'))
      raise 'runtime fallback warning missing' unless stderr.include?('XDG_RUNTIME_DIR is unset; using validated')
      raise 'session bus fallback warning missing' unless stderr.include?('DBUS_SESSION_BUS_ADDRESS is unset; using validated')
    end
    results.puts("#{name}=PASS"); results.flush
    puts "#{name}=PASS"
  end
end
