#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'tmpdir'
require 'timeout'
require 'shellwords'

desktop_variables = %w[DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET]
if desktop_variables.any? { |name| ENV[name] && !ENV[name].empty? }
  exec(desktop_variables.to_h { |name| [name, nil] }, RbConfig.ruby, __FILE__, *ARGV)
end
cgroup = File.read('/proc/self/cgroup')
abort 'run this proof from a worker/service outside a graphical app.scope or session.slice' if cgroup.include?('/session.slice/') || (cgroup.include?('/app.slice/') && cgroup.include?('.scope'))

binary, evidence = ARGV
abort 'usage: polkit_diagnostics_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
home = File.join(evidence, 'home')
temp_root = Dir.mktmpdir('kwin-mcp-263-', '/var/tmp')
%w[.config .local/share .cache .local/state .kde].each do |name|
  FileUtils.mkdir_p(File.join(home, name))
end
File.write(File.join(home, '.config', 'kglobalshortcutsrc'), "[kwin][Global Shortcuts]\nWindow Maximize=Meta+Backspace,Meta+Backspace,Maximize Window\n")
runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.euid}")
bus = ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime}/bus")
environment = {
  'HOME' => home,
  'XDG_CONFIG_HOME' => File.join(home, '.config'),
  'XDG_DATA_HOME' => File.join(home, '.local/share'),
  'XDG_CACHE_HOME' => File.join(home, '.cache'),
  'XDG_STATE_HOME' => File.join(home, '.local/state'),
  'KDEHOME' => File.join(home, '.kde'),
  'TMPDIR' => temp_root,
  'XDG_RUNTIME_DIR' => runtime,
  'DBUS_SESSION_BUS_ADDRESS' => bus,
  'WAYLAND_DISPLAY' => nil,
  'WAYLAND_SOCKET' => nil,
  'DISPLAY' => nil,
  'XAUTHORITY' => nil,
  'KWIN_MCP_REPO' => File.join(evidence, 'no-repository'),
  'CARGO' => '/usr/bin/false',
}

begin
  File.open(File.join(evidence, 'rpc.jsonl'), 'w') do |trace|
    File.open(File.join(evidence, 'server.stderr'), 'w') do |diagnostic|
      Open3.popen3(environment, binary, '--autoclean', '--width', '1600', '--height', '1000') do |input, output, error, server|
        stderr_reader = Thread.new { IO.copy_stream(error, diagnostic) }
        number = 0
        session_id = nil
        rpc = lambda do |method, params|
          number += 1
          request = {jsonrpc: '2.0', id: number, method: method, params: params}
          trace.puts(JSON.generate({request: request})); trace.flush
          input.puts(JSON.generate(request)); input.flush
          Timeout.timeout(60) do
            loop do
              line = output.gets or raise 'unexpected server EOF'
              message = JSON.parse(line)
              trace.puts(JSON.generate({response: message})); trace.flush
              break message if message['id'] == number
            end
          end
        end
        call = lambda do |name, arguments = {}|
          response = rpc.call('tools/call', {name: name, arguments: arguments})
          raise response.inspect if response['error']
          response.fetch('result')
        end
        content = lambda { |response| response.fetch('structuredContent') }

        begin
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'initial-maximize-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start', {browser_profiles: 'fresh', width: 1280, height: 800}))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          File.write(File.join(evidence, 'started.json'), JSON.pretty_generate(started))
          workdir = started.fetch('workdir')
          probe = File.join(workdir, 'polkit-probe.rb')
          report = File.join(workdir, 'polkit-report.json')
          FileUtils.cp(File.join(__dir__, 'fixtures', 'polkit_probe.rb'), probe)
          launched = call.call('launch_app', args.merge(command: "konsole --separate --hold -e ruby #{probe.shellescape} #{report.shellescape}"))
          raise launched.inspect if launched['isError']
          Timeout.timeout(20) { sleep 0.1 until File.file?(report) }
          observed = JSON.parse(File.read(report))
          raise observed.inspect unless observed['status'] == 'PASS'
          FileUtils.cp(report, File.join(evidence, 'verification.json'))
          FileUtils.cp(File.join(workdir, 'polkit-agent.log'), File.join(evidence, 'polkit-agent.log'))
          shot = call.call('screenshot', args)
          raise shot.inspect if shot['isError']
          FileUtils.cp(content.call(shot).fetch('path'), File.join(evidence, 'after.png'))
          warn "polkit=#{observed.inspect}"
          call.call('session_stop', args)
          session_id = nil
          puts 'polkit_diagnostics=PASS'
        rescue StandardError => error
          File.write(File.join(evidence, 'failure.txt'), "#{error.class}: #{error.message}\n")
          raise
        ensure
          begin
            call.call('session_stop', {session_id: session_id}) if session_id
          rescue StandardError => stop_error
            warn "session_stop: #{stop_error.message}"
          ensure
            input.close
            begin
              status = Timeout.timeout(15) { server.value }
            rescue Timeout::Error
              Process.kill('TERM', server.pid)
              server.value
              raise 'kwin-mcp did not exit after stdin closed'
            ensure
              stderr_reader.join
            end
            raise "kwin-mcp exited with #{status.exitstatus}" unless status.success?
          end
        end
      end
    end
  end
ensure
  FileUtils.rm_rf(home)
  FileUtils.rm_rf(temp_root)
end
