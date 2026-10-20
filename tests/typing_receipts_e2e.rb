#!/usr/bin/env ruby
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'tmpdir'
require 'timeout'
require 'uri'
require 'shellwords'

desktop_variables = %w[DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET]
if desktop_variables.any? { |name| ENV[name] && !ENV[name].empty? }
  exec(desktop_variables.to_h { |name| [name, nil] }, RbConfig.ruby, __FILE__, *ARGV)
end
cgroup = File.read('/proc/self/cgroup')
abort 'run this proof from a worker/service outside a graphical app.scope or session.slice' if cgroup.include?('/session.slice/') || (cgroup.include?('/app.slice/') && cgroup.include?('.scope'))

binary, evidence = ARGV
abort 'usage: typing_receipts_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
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
          initialized = rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'typing-receipts-proof', version: '1'}})
          raise initialized.inspect if initialized['error']
          input.puts(JSON.generate({jsonrpc: '2.0', method: 'notifications/initialized'})); input.flush

          started = content.call(call.call('session_start', {browser_profiles: 'fresh', width: 1280, height: 800}))
          session_id = started.fetch('session_id')
          args = {session_id: session_id}
          File.write(File.join(evidence, 'started.json'), JSON.pretty_generate(started))
          center = lambda do |name|
            deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
            loop do
              tree = call.call('accessibility_tree', args.merge(max_depth: 12))
              text = tree.fetch('content').filter_map { |item| item['text'] }.join("\n")
              line = text.lines.find { |row| fields = row.strip.split("\t"); %w[entry text].include?(fields[0]) && fields[1] == name && fields[2].to_s.include?('visible') }
              if line && (match = line.match(/\((-?\d+), (-?\d+), (\d+), (\d+)\)/))
                x, y, width, height = match.captures.map(&:to_i)
                break {x: x + width / 2, y: y + height / 2}
              end
              raise "could not locate #{name}: #{text}" if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
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
          html = '<!doctype html><meta charset="utf-8"><title>Typing receipt probe</title><style>body{font:20px sans-serif}input,textarea{display:block;width:1100px;margin:18px;font:18px monospace}textarea{height:260px}</style><h1>Typing receipt probe</h1><input type="search" aria-label="Unrestricted search" autocomplete="off"><input type="search" aria-label="Length-limited search" maxlength="72" autocomplete="off"><input aria-label="Zero-length page field" maxlength="0"><textarea aria-label="Markdown"></textarea>'
          url = 'data:text/html,' + URI::DEFAULT_PARSER.escape(html)
          call.call('launch_app', args.merge(command: "google-chrome-stable --no-first-run --no-default-browser-check #{Shellwords.escape(url)}"))
          snapshot.call('before')
          results = {}

          # A toolbar readback may be unsupported, but a full observed URL is verified.
          address = 'https://example.invalid/' + 'q' * 47
          raise address.length.inspect unless address.length == 71
          call.call('keyboard_key', args.merge(key: 'ctrl+l'))
          result = call.call('keyboard_type', args.merge(text: address))
          delivery = content.call(result).fetch('delivery')
          raise result.inspect if result['isError'] || !%w[verified unverified].include?(delivery['status'])
          observed = copy_value.call
          raise observed.inspect unless observed == address
          results['address_bar'] = {result: result, copied: observed}
          snapshot.call('address-bar')
          call.call('keyboard_key', args.merge(key: 'Escape'))

          query = 'from:account-security-noreply@accountprotection.microsoft.com newer_than:1d'
          [['Unrestricted search', 75], ['Length-limited search', 72]].each do |name, length|
            call.call('mouse_click', args.merge(center.call(name)))
            result = call.call('keyboard_type', args.merge(text: query))
            payload = content.call(result)
            delivery = payload.fetch('delivery')
            raise result.inspect unless delivery['expected_field_characters'] == 75 && delivery['observed_field_characters'] == length
            expected_status = length == 75 ? 'verified' : 'pending_or_mismatched'
            raise result.inspect unless delivery['status'] == expected_status && result['isError'] == (length != 75)
            context = payload.fetch('verification_context')
            raise context.inspect unless context['observed_count_normalization'] == 'none' && context['comparison_normalization'] == 'remove_u_fffc_only' && context['application_specific_normalization'] == false
            observed = copy_value.call
            raise observed.inspect unless observed == query[0, length]
            results[name] = {result: result, copied: observed}
          end
          # An empty web control must remain a mismatch, not a toolbar exemption.
          call.call('mouse_click', args.merge(center.call('Zero-length page field')))
          zero = call.call('keyboard_type', args.merge(text: 'abc'))
          raise zero.inspect unless zero['isError'] && content.call(zero).dig('delivery', 'status') == 'pending_or_mismatched' && content.call(zero).dig('delivery', 'observed_field_characters') == 0
          results['empty_page_field'] = zero

          markdown = ("# Receipt timing\n\n```ruby\nputs 'receipt proof'\n```\n\n" + (1..90).map { |n| "- Item #{n}: verify ordinary multiline ASCII input.\n" }.join)[0, 1700]
          call.call('mouse_click', args.merge(center.call('Markdown')))
          large = call.call('keyboard_type', args.merge(text: markdown))
          payload = content.call(large)
          raise large.inspect if large['isError'] || payload['submitted_characters'] != 1700 || !payload['submission_complete']
          raise payload.inspect unless payload.fetch('timings_ms').fetch('field_verification') <= 13_000
          observed = copy_value.call
          raise "multiline readback differed (#{observed.length} vs #{markdown.length})" unless observed == markdown
          results['multiline'] = {result: large, copied_characters: observed.length}
          snapshot.call('after-multiline')
          File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(results))
          call.call('session_stop', args)
          session_id = nil
          puts 'typing_receipts=PASS'
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
