#!/usr/bin/env ruby
# Run the built stdio entrypoint against private Konsole and Qt fixtures.
# Usage: ruby tests/keyboard_paste_e2e.rb SERVER ARTIFACTS [--baseline]
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'

class KeyboardPasteProbe
  def initialize(server, artifacts)
    @artifacts = File.expand_path(artifacts)
    @home = File.join(@artifacts, 'home')
    %w[.config .cache .local/state .local/share .kde].each do |name|
      FileUtils.mkdir_p(File.join(@home, name))
    end
    @log = File.open(File.join(@artifacts, 'protocol.jsonl'), 'w')
    @id = 0
    @session_id = nil
    environment = {
      'HOME' => @home, 'XDG_CONFIG_HOME' => @home + '/.config',
      'XDG_CACHE_HOME' => @home + '/.cache', 'XDG_DATA_HOME' => @home + '/.local/share',
      'XDG_STATE_HOME' => @home + '/.local/state', 'KDEHOME' => @home + '/.kde',
      'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
      'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{Process.euid}/bus",
      'KWIN_MCP_REPO' => File.join(@artifacts, 'no-source'), 'CARGO' => '/usr/bin/false'
    }
    @input, @output, errors, @process = Open3.popen3(
      environment, File.expand_path(server), '--autoclean', '--no-viewer'
    )
    @stderr = Thread.new do
      File.open(File.join(@artifacts, 'server.stderr.log'), 'w') do |file|
        errors.each_line { |line| file.write(line); file.flush }
      end
    end
    rpc('initialize', protocolVersion: '2024-11-05', capabilities: {},
        clientInfo: {name: 'keyboard-paste-e2e', version: '1'})
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
    @input.flush
    @tools = rpc('tools/list', {}).fetch('result').fetch('tools').map { |tool| tool.fetch('name') }
    started = data(call('session_start', width: 1232, height: 924, browser_profiles: 'fresh',
                                      startup_timeout_seconds: 120))
    @session_id = started.fetch('session_id')
  rescue StandardError
    stop
    raise
  end

  def rpc(method, params)
    @id += 1
    began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    @input.puts(JSON.generate(jsonrpc: '2.0', id: @id, method: method, params: params))
    @input.flush
    response = Timeout.timeout(150) do
      loop do
        line = @output.gets or raise 'server closed stdout'
        message = JSON.parse(line)
        break message if message['id'] == @id
      end
    end
    @log.puts(JSON.generate(method: method, params: params,
      elapsed_seconds: Process.clock_gettime(Process::CLOCK_MONOTONIC) - began, response: response))
    @log.flush
    response
  end

  def call(name, arguments = {})
    arguments = arguments.merge(session_id: @session_id) if @session_id
    rpc('tools/call', name: name, arguments: arguments)
  end

  def data(response)
    raise "MCP failure: #{response}" if response['error'] || response.dig('result', 'isError')
    response.fetch('result').fetch('structuredContent')
  end

  def screen(name)
    shot = data(call('screenshot'))
    FileUtils.cp(shot.fetch('path'), File.join(@artifacts, "#{name}.png"))
  end

  def wait_for_focus(window_id, application, roles)
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 15
    loop do
      windows = data(call('window_list')).fetch('windows')
      response = call('accessibility_tree', app_name: application, show_elements: true)
      data(response)
      tree = response.fetch('result').fetch('content').filter_map { |block| block['text'] }.join("\n")
      File.write(File.join(@artifacts, "#{application}-focus.txt"), tree)
      focused = tree.lines.any? do |line|
        role, _name, states = line.strip.split("\t", -1)
        roles.include?(role) && %w[focused visible].all? { |state| states.to_s.split('|').include?(state) }
      end
      return if focused && windows.any? { |window| window['id'] == window_id && window['active'] }
      raise "#{application} fixture did not receive keyboard focus" if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
      sleep 0.2
    end
  end

  def export_text(name, expected)
    session_path = "#{@home}/#{name}"
    data(call('wait_for_file', session_path: session_path, timeout_seconds: 15, stable_for_ms: 100))
    host_path = File.join(@artifacts, "#{name}.txt")
    data(call('export_file', session_path: session_path, host_path: host_path))
    observed = File.read(host_path, encoding: 'UTF-8')
    raise "independent #{name} value differs: #{observed.inspect}" unless observed == expected
  end

  def paste(text, name, terminal: false)
    began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    result = data(call('keyboard_paste', text: text))
    elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - began
    raise "paste was not acknowledged: #{result}" unless result['shortcut_submitted'] &&
      result['submission_complete'] && result['compositor_acknowledged'] &&
      result.dig('delivery', 'compositor_acknowledged')
    raise "clipboard length mismatch: #{result}" unless result['clipboard_characters'] == text.length
    raise "paste suggested blind retry: #{result}" unless result['retry_safe'] == false
    raise "paste overstated application delivery: #{result}" unless result['application_text'] == 'unverified' &&
      result['form_submission'] == 'not_observed'
    if terminal
      raise "terminal falsely claims text verification: #{result}" unless result.dig('delivery', 'status') == 'unverified'
      raise "terminal paste took #{elapsed.round(3)} seconds" unless elapsed < 5
    end
    File.write(File.join(@artifacts, "#{name}-receipt.json"),
      JSON.pretty_generate(result.merge('wall_seconds' => elapsed)))
    elapsed
  end

  def run(baseline)
    launched = data(call('launch_app', command: 'konsole --separate --nofork -e bash --noprofile --norc'))
    wait_for_focus(launched.fetch('window'), 'konsole', ['terminal'])
    screen('terminal-before')
    command = %q{printf '%s' 'terminal-paste-proof' > "$HOME/clipboard-terminal-proof"; #}.ljust(120, 'x')
    if baseline
      raise 'baseline unexpectedly advertises keyboard_paste' if @tools.include?('keyboard_paste')
      began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      receipt = data(call('keyboard_type', text: command))
      elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - began
      File.write(File.join(@artifacts, 'baseline-timing.json'),
        JSON.pretty_generate(characters: command.length, wall_seconds: elapsed, receipt: receipt))
    else
      raise 'keyboard_paste is missing from tools/list' unless @tools.include?('keyboard_paste')
      elapsed = paste(command, 'terminal-ascii', terminal: true)
    end
    data(call('keyboard_key', key: 'Return'))
    export_text('clipboard-terminal-proof', 'terminal-paste-proof')
    screen('terminal-after')
    if baseline
      raise "REGRESSION #476: keyboard_paste is unavailable; 120-character paced input took #{elapsed.round(3)} seconds"
    end

    value = "caf\u00e9 \u03a9\n\u6771\u4eac \u{1f642}"
    command = "printf '%s' '#{value}' > \"$HOME/clipboard-terminal-utf8\""
    paste(command, 'terminal-utf8-multiline', terminal: true)
    data(call('keyboard_key', key: 'Return'))
    export_text('clipboard-terminal-utf8', value)

    data(call('clipboard_write', target: 'session', text: 'empty-input-sentinel'))
    rejected = call('keyboard_paste', text: '')
    raise "empty paste was not rejected before input: #{rejected}" unless rejected.dig('error', 'code') == -32602
    raise 'empty paste changed the clipboard' unless data(call('clipboard_read')).fetch('text') == 'empty-input-sentinel'

    launched = data(call('launch_app',
      command: %q{kdialog --title "Clipboard paste fixture" --inputbox "Paste fixture" > "$HOME/clipboard-field-proof"}))
    wait_for_focus(launched.fetch('window'), 'kdialog', %w[entry text])
    screen('field-before')
    value = "caf\u00e9 \u03a9 \u6771\u4eac \u{1f642}"
    paste(value, 'field-utf8')
    screen('field-after')
    data(call('keyboard_key', key: 'Return'))
    export_text('clipboard-field-proof', value + "\n")
    puts "PASS: fast terminal paste, UTF-8/multiline command, Qt input value, and empty-input guard; #{@artifacts}"
  end

  def stop
    if @session_id
      data(call('session_stop'))
      @session_id = nil
    end
  ensure
    @input&.close unless @input&.closed?
    if @process
      begin
        Timeout.timeout(45) { @process.value }
      rescue Timeout::Error
        Process.kill('TERM', @process.pid) if @process.alive?
        Timeout.timeout(15) { @process.value }
      end
    end
    @stderr&.join(5)
    @log&.close
  end
end

probe = nil
begin
  probe = KeyboardPasteProbe.new(ARGV.fetch(0), ARGV.fetch(1))
  probe.run(ARGV[2] == '--baseline')
ensure
  probe&.stop
end
