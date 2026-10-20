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
abort 'usage: devtools_placeholder_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
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
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'devtools-placeholder-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start', {browser_profiles: 'fresh', width: 1280, height: 800}))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          File.write(File.join(evidence, 'started.json'), JSON.pretty_generate(started))
          center = lambda do |role, name|
            deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
            loop do
              tree = call.call('accessibility_tree', args.merge(max_depth: 12))
              text = tree.fetch('content').filter_map { |item| item['text'] }.join("\n")
              line = text.lines.find { |row| fields = row.strip.split("\t"); fields[0] == role && fields[1] == name && fields[2].to_s.include?('visible') }
              if line && (match = line.match(/\((-?\d+), (-?\d+), (\d+), (\d+)\)/))
                x, y, width, height = match.captures.map(&:to_i)
                break {x: x + width / 2, y: y + height / 2}
              end
              raise "could not locate #{role} #{name}" if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
              sleep 0.1
            end
          end
          snapshot = lambda do |name|
            result = call.call('screenshot', args)
            FileUtils.cp(content.call(result).fetch('path'), File.join(evidence, "#{name}.png"))
          end
          copy_value = lambda do
            call.call('keyboard_key', args.merge(key: 'ctrl+a'))
            call.call('keyboard_key', args.merge(key: 'ctrl+c'))
            content.call(call.call('clipboard_read', args)).fetch('text')
          end
          call.call('launch_app', args.merge(command: 'google-chrome-stable --no-first-run --no-default-browser-check --auto-open-devtools-for-tabs about:blank'))
          call.call('mouse_click', args.merge(center.call('page tab', 'Network')))
          call.call('mouse_click', args.merge(center.call('entry', 'Filter')))
          snapshot.call('before-empty-filter')
          placeholder = call.call('keyboard_type', args.merge(text: 'conversation'))
          delivery = content.call(placeholder).fetch('delivery')
          raise placeholder.inspect if placeholder['isError'] || delivery['status'] != 'unverified' || delivery['method'] != 'atspi_placeholder_transition'
          raise delivery.inspect unless delivery['expected_field_characters'] == 12 && delivery['observed_field_characters'] == 12
          snapshot.call('after-placeholder')
          observed = copy_value.call
          raise observed.inspect unless observed == 'conversation'
          # A genuine value equal to the accessible label must still be retained.
          call.call('keyboard_type', args.merge(text: 'Filter'))
          call.call('keyboard_key', args.merge(key: 'End'))
          real_value = call.call('keyboard_type', args.merge(text: 'conversation'))
          actual = content.call(real_value).fetch('delivery')
          raise real_value.inspect if real_value['isError'] || actual['status'] != 'verified'
          raise actual.inspect unless actual['expected_field_characters'] == 18 && actual['observed_field_characters'] == 18
          snapshot.call('after-genuine-value')
          observed_real = copy_value.call
          raise observed_real.inspect unless observed_real == 'Filterconversation'
          File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate({placeholder: placeholder, placeholder_copied: observed, genuine_value: real_value, genuine_value_copied: observed_real}))
          call.call('session_stop', args)
          session_id = nil
          puts 'devtools_placeholder=PASS'
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
