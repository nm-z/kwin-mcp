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
      Open3.popen3(environment, binary, '--autoclean', '--width', '1920', '--height', '1080') do |input, output, error, server|
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
          raise "new window did not start maximized: #{initial_geometry.inspect}" unless initial_geometry == { 'x' => 0, 'y' => 0, 'w' => 1920, 'h' => 1080 }

          content.call(call.call('window_activate', args.merge(window_id: before.fetch('id'))))
          before_capture = content.call(call.call('screenshot', args))
          FileUtils.cp(before_capture.fetch('path'), File.join(evidence, 'before-maximized.png'))
          content.call(call.call('keyboard_key', args.merge(key: 'alt+F3')))
          menu_capture = content.call(call.call('screenshot', args))
          FileUtils.cp(menu_capture.fetch('path'), File.join(evidence, 'maximize-menu.png'))
          # Maximize is the first item in KWin's window menu; use the same mouse
          # toggle path as #510, without installing a custom keyboard shortcut.
          content.call(call.call('mouse_click', args.merge(x: 90, y: 12)))
          restore_deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
          restored = loop do
            current = content.call(call.call('window_list', args)).fetch('windows').find { |window| window['id'] == before['id'] }
            raise 'Konsole window disappeared after restore' unless current
            break current if current['w'] < 1920 && current['h'] < 1080
            if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= restore_deadline
              failed_capture = content.call(call.call('screenshot', args))
              FileUtils.cp(failed_capture.fetch('path'), File.join(evidence, 'restore-failed.png'))
              raise "KWin did not restore the maximized window: #{current.slice('w', 'h').inspect}"
            end
            sleep 0.1
          end
          # Keep observing after the menu has closed, catching forced rules that
          # briefly restore a window before snapping it back to the display size.
          5.times do
            sleep 0.25
            current = content.call(call.call('window_list', args)).fetch('windows').find { |window| window['id'] == before['id'] }
            raise "restored window snapped back: #{current.inspect}" unless current && current.slice('w', 'h') == restored.slice('w', 'h')
          end
          restored_capture = content.call(call.call('screenshot', args))
          FileUtils.cp(restored_capture.fetch('path'), File.join(evidence, 'after-menu-restore.png'))
          raise 'restore screenshot has the wrong window identity' unless restored_capture.dig('window', 'id') == before['id']

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

          native = content.call(call.call('session_start', {window_mode: 'native'}))
          session_id = native.fetch('session_id')
          args = {session_id: session_id}
          native_rules = File.read(File.join(native.fetch('workdir'), 'kwinrulesrc'))
          raise 'native session still writes maximize rules' if native_rules.include?('maximize')
          content.call(call.call('launch_app', args.merge(command: 'konsole --separate')))
          native_window = content.call(call.call('window_list', args)).fetch('windows').find do |window|
            window['resourceClass'] == 'org.kde.konsole'
          end
          raise 'native Konsole window not found after launch' unless native_window
          raise "native window opened at the full display: #{native_window.slice('x', 'y', 'w', 'h').inspect}" if native_window['w'] == 1920 && native_window['h'] == 1080
          File.write(File.join(evidence, 'native.json'), JSON.pretty_generate(native: native_window))
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
