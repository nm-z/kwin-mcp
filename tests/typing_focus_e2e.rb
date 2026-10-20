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
abort 'usage: typing_focus_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
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
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'typing-focus-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start', {browser_profiles: 'fresh', width: 1280, height: 800}))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          File.write(File.join(evidence, 'started.json'), JSON.pretty_generate(started))
          checked = lambda do |name, extra = {}|
            result = call.call(name, args.merge(extra))
            raise "#{name}: #{result.inspect}" if result['isError']
            result
          end
          key = ->(value) { checked.call('keyboard_key', key:value) }
          capture = lambda do |label|
            result = checked.call('screenshot')
            FileUtils.cp(content.call(result).fetch('path'), File.join(evidence,"#{label}.png"))
          end
          copy_focused = lambda do
            key.call('ctrl+a')
            key.call('ctrl+c')
            sleep 0.15
            content.call(checked.call('clipboard_read')).fetch('text')
          end
          wait_active_title = lambda do |title|
            Timeout.timeout(15) do
              loop do
                windows = content.call(checked.call('window_list')).fetch('windows')
                window = windows.find { |w| w['active'] && w['title'].include?(title) }
                break window if window
                sleep 0.1
              end
            end
          end
          journal_path = started.fetch('input_log')
          request_keys = lambda do |request_id|
            next [] unless File.file?(journal_path)
            File.foreach(journal_path).filter_map do |line|
              next unless line.end_with?("\n")
              row = JSON.parse(line)
              next unless row['event'] == 'key'
              tool = row['tool'] || {}
              next unless tool['name'] == 'keyboard_type' && tool['request_id'].to_s == request_id.to_s
              row
            end
          end
          raw_send = lambda do |name, extra|
            number += 1
            request = {jsonrpc:'2.0',id:number,method:'tools/call',params:{name:name,arguments:args.merge(extra)}}
            trace.puts(JSON.generate(request:request)); trace.flush
            input.puts(JSON.generate(request)); input.flush
            number
          end
          raw_receive = lambda do |wanted|
            Timeout.timeout(35) do
              loop do
                line = output.gets or raise 'MCP stdout closed'
                packet = JSON.parse(line)
                trace.puts(JSON.generate(response:packet)); trace.flush
                next unless packet.key?('id')
                raise "unexpected response #{packet.inspect}" unless packet['id'] == wanted && !packet.key?('method')
                break packet
              end
            end
          end
          results = []
          [:window, :tab].each do |mode|
            tag = "412-#{mode}-#{Process.pid}"
            dir = File.join(started.fetch('workdir'),tag)
            FileUtils.mkdir_p(dir)
            a_title, b_title = "Focus guard A #{tag}", "Focus guard B #{tag}"
            gate = File.join(dir,'open-b')
            profile = File.join(dir,'profile')
            page = lambda do |title|
              "<!doctype html><meta charset='utf-8'><title>#{title}</title><style>body{margin:24px;font:20px sans-serif}textarea{display:block;width:900px;height:420px;margin-top:20px;font:24px monospace}</style><h1>#{title}</h1><textarea autofocus aria-label='#{title} text'></textarea>"
            end
            a_path, b_path = File.join(dir,'a.html'), File.join(dir,'b.html')
            File.write(a_path,page.call(a_title))
            File.write(b_path,page.call(b_title))
            chrome = ['google-chrome-stable','--no-first-run','--no-default-browser-check','--disable-background-networking','--force-renderer-accessibility',"--user-data-dir=#{profile}"]
            a_command = (chrome+['--new-window',"file://#{a_path}"]).shelljoin
            b_command = (chrome+(mode == :window ? ['--new-window'] : [])+["file://#{b_path}"]).shelljoin
            script = File.join(dir,'launch.sh')
            File.write(script, <<~BASH)
              #!/usr/bin/env bash
              set -euo pipefail
              (
                deadline=$((SECONDS + 90))
                while [[ ! -f #{gate.shellescape} ]]; do
                  if (( SECONDS >= deadline )); then exit 0; fi
                  sleep 0.02
                done
                exec #{b_command}
              ) >#{File.join(dir,'b-launch.log').shellescape} 2>&1 &
              exec #{a_command}
            BASH
            File.chmod(0o700,script)
            checked.call('launch_app', command:['bash',script].shelljoin)
            a_window = wait_active_title.call(a_title)
            a_id = a_window.fetch('id')
            stable = 'Stable-UPPER-and-lower'
            stable_result = content.call(checked.call('keyboard_type',text:stable))
            raise "stable typing unverified: #{stable_result.inspect}" unless stable_result.dig('delivery','status') == 'verified'
            raise 'stable native copy differs' unless copy_focused.call == stable
            key.call('ctrl+a')
            key.call('Backspace')
            capture.call("#{tag}-before")
            payload = 'A'*400
            typing_id = raw_send.call('keyboard_type',text:payload)
            progress = Timeout.timeout(12) do
              loop do
                rows = request_keys.call(typing_id)
                presses = rows.count { |r| r['pressed'] && r['character']=='A' && r['delivery']=='flushed' }
                held = {}
                rows.each { |r| held[r.fetch('keycode')]=r.fetch('pressed') if r['delivery']=='flushed' }
                break {submitted_a:presses,held_keys:held.select { |_k,v| v }.keys} if presses>=8 && (held[42] || held[54])
                sleep 0.01
              end
            end
            gate_time = Process.clock_gettime(Process::CLOCK_MONOTONIC)
            File.write(gate,"go\n")
            packet = raw_receive.call(typing_id)
            elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-gate_time
            data = packet.fetch('error').fetch('data')
            raise "wrong cancellation: #{packet.inspect}" unless data['reason']=='target_changed'
            submitted,attempted = data.values_at('submitted_characters','attempted_characters')
            raise 'missing partial submission' unless submitted.is_a?(Integer) && attempted.is_a?(Integer) && submitted>0 && submitted<payload.length && attempted>=submitted && attempted<payload.length
            raise 'unsafe cancellation flags' unless data['submission_complete']==false && data['retry_safe']==false
            keys = Timeout.timeout(3) do
              loop do
                rows = request_keys.call(typing_id)
                held = {}
                rows.each { |r| held[r.fetch('keycode')]=r.fetch('pressed') if r['delivery']=='flushed' }
                shift = rows.any? { |r| [42,54].include?(r['keycode']) && r['pressed'] && r['delivery']=='flushed' }
                break rows if shift && held.values.none?
                sleep 0.01
              end
            end
            raise 'uncertain release delivery' unless keys.all? { |r| r['delivery']=='flushed' }
            File.write(File.join(evidence,"#{tag}-interruption.json"),JSON.pretty_generate(rpc:packet,progress_before_gate:progress,seconds_from_gate_to_response:elapsed,key_events:keys))
            b_window = wait_active_title.call(b_title)
            b_id = b_window.fetch('id')
            raise 'same-window case changed windows' if mode==:tab && b_id!=a_id
            raise 'new-window case reused window' if mode==:window && b_id==a_id
            capture.call("#{tag}-after-cancel")
            # No key tool runs between interruption and this lowercase follow-up.
            lower = 'cleanlowercase'
            lower_result = content.call(checked.call('keyboard_type',text:lower))
            raise 'lowercase follow-up unverified' unless lower_result.dig('delivery','status')=='verified'
            b_value = copy_focused.call
            raise "lowercase or destination changed: #{b_value.inspect}" unless b_value.end_with?(lower) && b_value.delete_suffix(lower).match?(/\AA*\z/)
            leaked = b_value.length-lower.length
            if mode==:tab
              key.call('ctrl+shift+Tab')
            else
              checked.call('window_activate',window_id:a_id)
            end
            wait_active_title.call(a_title)
            a_value = copy_focused.call
            raise 'original field lacks uppercase prefix' unless a_value.match?(/\AA+\z/)
            raise 'full payload arrived despite cancellation' unless a_value.length+leaked<payload.length
            capture.call("#{tag}-original-field")
            result = {mode:mode,window_before:a_id,window_after:b_id,requested:payload.length,submitted:submitted,attempted:attempted,a_value:a_value,b_value:b_value,characters_observed_in_b_before_followup:leaked,lowercase_followup_exact:true,cancellation_seconds:elapsed}
            results << result
            File.write(File.join(evidence,"#{tag}-native-values.json"),JSON.pretty_generate(result))
          end
          button_dir = File.join(started.fetch('workdir'),'noneditable-412')
          FileUtils.mkdir_p(button_dir)
          button_title = 'Noneditable target 412'
          button = File.join(button_dir,'button.html')
          File.write(button,"<!doctype html><title>#{button_title}</title><body style='margin:40px'><button autofocus style='font:28px sans-serif;padding:24px'>Noneditable preflight target</button>")
          checked.call('launch_app',command:['google-chrome-stable','--no-first-run','--force-renderer-accessibility',"--user-data-dir=#{button_dir}/profile",'--new-window',"file://#{button}"].shelljoin)
          wait_active_title.call(button_title)
          capture.call('button-before')
          button_id = raw_send.call('keyboard_type',text:'MUSTSTAYEMPTY')
          packet = raw_receive.call(button_id)
          data = packet.fetch('error').fetch('data')
          raise "wrong preflight #{packet.inspect}" unless data['reason']=='text_target_unavailable' && data['submitted_characters']==0 && data['attempted_characters']==0 && data['retry_safe']==true
          raise 'preflight generated key presses' if request_keys.call(button_id).any? { |r| r['pressed'] }
          capture.call('button-after')
          File.write(File.join(evidence,'button-preflight.json'),JSON.pretty_generate(packet))
          FileUtils.cp(journal_path,File.join(evidence,'input.jsonl'))
          File.write(File.join(evidence,'verification.json'),JSON.pretty_generate(cases:results,noneditable_preflight:data))
          call.call('session_stop', args)
          session_id = nil
          puts 'typing_focus=PASS'
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
