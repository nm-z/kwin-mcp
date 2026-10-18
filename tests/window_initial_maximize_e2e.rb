#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'tmpdir'
require 'timeout'

desktop_variables = %w[DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET]
if desktop_variables.any? { |name| ENV[name] && !ENV[name].empty? }
  exec(desktop_variables.to_h { |name| [name, nil] }, RbConfig.ruby, __FILE__, *ARGV)
end
cgroup = File.read('/proc/self/cgroup')
abort 'run this proof from a worker/service outside a graphical app.scope or session.slice' if cgroup.include?('/session.slice/') || (cgroup.include?('/app.slice/') && cgroup.include?('.scope'))

binary, evidence = ARGV
abort 'usage: window_initial_maximize_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
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
          raise response.inspect if response['error'] || response.dig('result', 'isError')
          response.fetch('result')
        end
        content = lambda { |response| response.fetch('structuredContent') }

        begin
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'initial-maximize-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start'))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          rules = File.read(File.join(started.fetch('workdir'), 'kwinrulesrc'))
          raise 'KWin maximize rules are not initial-only' unless rules.include?("maximizehorizrule=3") && rules.include?("maximizevertrule=3")
          content.call(call.call('launch_app', args.merge(command: 'konsole --separate')))
          before = content.call(call.call('window_list', args)).fetch('windows').find do |window|
            window['resourceClass'] == 'org.kde.konsole'
          end
          raise 'Konsole window not found after launch' unless before
          initial_geometry = before.slice('x', 'y', 'w', 'h')
          raise "new window did not start maximized: #{initial_geometry.inspect}" unless initial_geometry == { 'x' => 0, 'y' => 0, 'w' => 1600, 'h' => 1000 }

          content.call(call.call('window_activate', args.merge(window_id: before.fetch('id'))))
          content.call(call.call('keyboard_key', args.merge(key: 'super+Backspace')))
          restored = content.call(call.call('window_list', args)).fetch('windows').find do |window|
            window['id'] == before['id']
          end
          raise 'Konsole window disappeared after restore' unless restored
          raise "KWin did not restore the maximized window: #{restored.slice('w', 'h').inspect}" if restored['w'] >= 1600 || restored['h'] >= 1000

          drag = {
            button: 'right',
            from_x: restored.fetch('w').round - 10,
            from_y: restored.fetch('h').round - 10,
            to_x: restored.fetch('w').round - 110,
            to_y: restored.fetch('h').round - 110,
          }
          pressed = false
          begin
            content.call(call.call('keyboard_press', args.merge(key: 'super')))
            pressed = true
            content.call(call.call('mouse_drag', args.merge(drag)))
          ensure
            content.call(call.call('keyboard_release', args.merge(key: 'super'))) if pressed
          end

          resized = content.call(call.call('window_list', args)).fetch('windows').find do |window|
            window['id'] == before['id']
          end
          raise 'Konsole window disappeared after drag resize' unless resized
          if resized['w'] > restored['w'] - 75 || resized['h'] > restored['h'] - 75
            raise "native resize gesture did not shrink the window: #{resized.slice('w', 'h').inspect}"
          end

          content.call(call.call('window_activate', args.merge(window_id: resized.fetch('id'))))
          capture = content.call(call.call('screenshot', args))
          screenshot_geometry = capture.fetch('window').slice('width', 'height')
          raise "screenshot metadata differs from compositor geometry: #{screenshot_geometry.inspect}" unless screenshot_geometry == resized.slice('w', 'h').transform_keys { |key| key == 'w' ? 'width' : 'height' }
          raise "screenshot dimensions differ: #{capture.slice('width', 'height').inspect}" unless capture.slice('width', 'height') == screenshot_geometry

          File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(initial: before, restored: restored, resized: resized, screenshot: capture))
          content.call(call.call('session_stop', args))
          session_id = nil
          puts 'window_initial_maximize_e2e=PASS'
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
