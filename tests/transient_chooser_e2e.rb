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
abort 'usage: transient_chooser_e2e.rb /path/to/kwin-mcp /path/to/evidence [baseline|candidate]' unless binary && evidence
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
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'transient-chooser-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start', {browser_profiles: 'fresh', width: 1280, height: 800}))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          File.write(File.join(evidence, 'started.json'), JSON.pretty_generate(started))
          mode = ARGV.fetch(2, 'candidate')
          data_url = 'data:text/html,<title>Chooser capture parent</title><body style="background:%23244068;color:white;font:36px sans-serif"><h1>PARENT WINDOW</h1><p>The file chooser must cover this page.</p>'
          launched = call.call('launch_app', args.merge(command: ['google-chrome-stable', '--new-window', '--no-first-run', "--user-data-dir=#{started.fetch('workdir')}/chooser-profile", data_url].shelljoin))
          raise launched.inspect if launched['isError']
          capture = lambda do |label|
            result = rpc.call('tools/call', {name:'screenshot', arguments:args})
            File.write(File.join(evidence, "#{label}.json"), JSON.pretty_generate(result))
            path = result.dig('result','structuredContent','path')
            FileUtils.cp(path, File.join(evidence, "#{label}.png")) if path
            result
          end
          parent = content.call(call.call('window_list', args)).fetch('windows').find { |window| window['active'] }
          raise 'no parent' unless parent
          before = capture.call('before-parent')
          raise before.inspect if before['error']
          call.call('keyboard_key', args.merge(key:'ctrl+o'))
          chooser = nil
          Timeout.timeout(10) do
            loop do
              windows = content.call(call.call('window_list', args)).fetch('windows')
              chooser = windows.find { |window| window['active'] && window['id'] != parent['id'] && %w[dialog modal transient].any? { |flag| window[flag] } }
              break if chooser
              sleep 0.1
            end
          end
          File.write(File.join(evidence,'chooser.json'), JSON.pretty_generate(chooser))
          observed = capture.call('after-chooser')
          if mode == 'candidate'
            raise "stable chooser was rejected: #{observed.inspect}" if observed['error']
              data = observed.fetch('result').fetch('structuredContent')
              check = data.fetch('transient_pixel_check')
              raise data.inspect unless data.dig('window','id') == chooser['id'] && check['status'] == 'matches_window_capture' && check['reference_window_id'] == chooser['id'] && check['compared_pixels'] > 0 && check['mismatching_pixels'] == 0
          else
            raise observed.inspect if observed['error']
          end
          call.call('mouse_move', args.merge(x:50,y:50))
          # Begin a settling screenshot, then close only our chooser while the
          # capture is pending. Read both RPC responses without concurrent IO.
          number += 1
          screenshot_id = number
          request = {jsonrpc:'2.0',id:number,method:'tools/call',params:{name:'screenshot',arguments:args}}
          trace.puts(JSON.generate({request:request})); trace.flush
          input.puts(JSON.generate(request)); input.flush
          sleep 0.05
          number += 1
          close_id = number
          request = {jsonrpc:'2.0',id:number,method:'tools/call',params:{name:'keyboard_key',arguments:args.merge(key:'Escape')}}
          trace.puts(JSON.generate({request:request})); trace.flush
          input.puts(JSON.generate(request)); input.flush
          replies = {}
          Timeout.timeout(30) do
            until replies.key?(screenshot_id) && replies.key?(close_id)
              line = output.gets or raise 'server EOF'
              message = JSON.parse(line)
              trace.puts(JSON.generate({response:message})); trace.flush
              replies[message['id']] = message if [screenshot_id,close_id].include?(message['id'])
            end
          end
          race = replies.fetch(screenshot_id)
          File.write(File.join(evidence,'closing-chooser-capture.json'), JSON.pretty_generate(race))
          if mode == 'candidate'
            raise "changed chooser capture was accepted: #{race.inspect}" unless %w[transient_capture_unverified capture_target_changed capture_target_unavailable].include?(race.dig('error','data','reason'))
            raise 'rejected capture included image' if race.dig('result','structuredContent','path')
          elsif (path = race.dig('result','structuredContent','path'))
            FileUtils.cp(path, File.join(evidence,'before-race-misattribution.png'))
          end
          restored = capture.call('after-parent-restored')
          raise restored.inspect if restored['error']
          raise 'wrong parent after chooser close' unless restored.dig('result','structuredContent','window','id') == parent['id']
          cropped = call.call('screenshot', args.merge(region:[-10,-10,100,100]))
          raise cropped.inspect if cropped['isError']
          File.write(File.join(evidence,'verification.json'), JSON.pretty_generate({
            mode:mode, parent_id:parent['id'],chooser:chooser,
            chooser_capture:observed, closing_capture:race,
            parent_restored:true, negative_region:true,
          }))
          call.call('session_stop', args)
          session_id = nil
          puts 'transient_chooser=PASS'
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
