#!/usr/bin/env ruby
# Real production stdio path, using private session profiles and synthetic text.
# No compilation: KWIN_MCP_E2E_SERVER must name an already built private binary.
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'
require_relative 'electron_text_fixture'

class InputDeliveryProbe
  def initialize(server, artifacts)
    @artifacts = artifacts
    @baseline = ENV['KWIN_MCP_E2E_BASELINE'] == '1'
    @home = File.join(artifacts, 'home')
    %w[.config .cache .local/state .local/share .kde].each { |name| FileUtils.mkdir_p(File.join(@home,name)) }
    FileUtils.mkdir_p(artifacts)
    @log = File.open(File.join(artifacts, 'rpc.jsonl'), 'w')
    @id = 0
    @session_id = nil
    @started = false
    @input, @output, errors, @process = Open3.popen3(
{ 'KWIN_MCP_REPO' => File.join(artifacts, 'no-source'), 'CARGO'=>'/usr/bin/false',
  'HOME'=>@home,'XDG_CONFIG_HOME'=>@home+'/.config','XDG_CACHE_HOME'=>@home+'/.cache',
  'XDG_DATA_HOME'=>@home+'/.local/share','XDG_STATE_HOME'=>@home+'/.local/state',
  'KDEHOME'=>@home+'/.kde','XDG_RUNTIME_DIR'=>"/run/user/#{Process.euid}",
  'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=/run/user/#{Process.euid}/bus" },
      server, '--width', '1232', '--height', '924', '--autoclean'
    )
    @stderr = Thread.new do
      File.open(File.join(artifacts, 'server.stderr.log'), 'w') do |file|
        errors.each_line { |line| file.write(line); file.flush }
      end
    end
    rpc('initialize', protocolVersion: '2024-11-05', capabilities: {},
        clientInfo: { name: 'input-delivery-e2e', version: '1' })
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
    @input.flush
    start = call('session_start', width: 1232, height: 924)
    assert_success(start)
    @started = true
    @workdir = start.dig('result','structuredContent','workdir')
    @scope = start.dig('result','structuredContent','memory_limits','unit')
    @session_id = start.dig('result', 'structuredContent', 'session_id')
    raise 'server lacks per-session IDs' unless @session_id
  rescue StandardError
    stop
    raise
  end

  def rpc(method, params)
    @id += 1
    begin_time = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    @input.puts(JSON.generate(jsonrpc: '2.0', id: @id, method: method, params: params))
    @input.flush
    response = Timeout.timeout(240) do
      loop do
        line = @output.gets or raise 'server closed stdout'
        message = JSON.parse(line)
        break message if message['id'] == @id
      end
    end
    elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - begin_time
    @log.puts(JSON.generate(method: method, params: params, elapsed: elapsed, response: response))
    @log.flush
    response
  end

  def call(name, arguments = {})
    arguments = arguments.merge(session_id: @session_id) if @session_id
    rpc('tools/call', name: name, arguments: arguments)
  end

  def assert_success(response)
    raise "MCP failure: #{response}" if response['error'] || response.dig('result', 'isError')
    response.dig('result', 'structuredContent')
  end

  def screen(tag)
    data = assert_success(call('screenshot'))
    FileUtils.cp(data.fetch('path'), File.join(@artifacts, "#{tag}.png"))
    data
  end

  def tree
    response = call('accessibility_tree')
    assert_success(response)
    response.fetch('result').fetch('content').filter_map { |block| block['text'] }.join("\n")
  end

  def electron
    debug_port = ElectronTextFixture.free_port
    command = '/usr/lib/chatgpt/ChatGPT --user-data-dir="$HOME/.km-input-e2e-private" --force-renderer-accessibility' + " --remote-debugging-port=#{debug_port}"
    assert_success(call('launch_app', command: command))
    fixture = ElectronTextFixture.install(debug_port)
    File.write(File.join(@artifacts, 'electron-document.json'), JSON.pretty_generate(fixture))
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 60
    visible = nil
    loop do
      visible = tree
      break if visible.lines.any? { |line| line.include?("entry\tDo anything\t") }
      raise 'Electron composer did not become accessible' if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
      sleep 0.5
    end
    # The composer starts focused; an explicit click verifies active-window routing.
    match = visible.match(/entry\tDo anything\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'composer bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    [550, 250].each do |length|
      assert_success(call('keyboard_key', key: 'ctrl+a'))
      assert_success(call('keyboard_key', key: 'Backspace'))
      text = ("Paced delivery probe: letters AaZz digits 0123456789. " * 11)[0, length]
      File.write(File.join(@artifacts, "expected-#{length}.txt"), text)
      result = assert_success(call('keyboard_type', text: text))
      unless @baseline
        raise 'typing omitted completed submission' unless result['submission_complete']
        raise 'typing suggested blind retry' unless result['retry_safe'] == false
        delivery = result.fetch('delivery')
        raise "unconfirmed Electron text: #{delivery}" unless delivery['status'] == 'verified'
        raise "missing compositor receipt: #{delivery}" unless delivery['compositor_acknowledged']
        raise "wrong expected length: #{delivery}" unless delivery['expected_field_characters'] == length
        raise "wrong observed length: #{delivery}" unless delivery['observed_field_characters'] == length
      end
      # Separate public accessibility read after keyboard_type returns.
      visible = tree
      File.write(File.join(@artifacts, "electron-#{length}.tree.txt"), visible)
      raise 'independent tree lacks complete text' unless visible.include?(text)
      screen("electron-#{length}-after-return")
    end
    # Rejection must precede all input, leaving the verified field unchanged.
    invalid = call('keyboard_type', text: "valid prefix\u00e9")
    raise "non-ASCII input was accepted: #{invalid}" unless invalid.dig('error', 'code') == -32602
    raise 'invalid input changed field' if tree.include?('valid prefix')
    assert_success(call('keyboard_key', key: 'ctrl+a'))
    assert_success(call('keyboard_key', key: 'Backspace'))
  end

  def terminal
    assert_success(call('launch_app', command: 'konsole --separate --nofork -e bash --noprofile --norc'))
    command = 'printf %s terminal-paced-proof > "$HOME/km-input-terminal-proof"'
    data = assert_success(call('keyboard_type', text: command))
    delivery = data.fetch('delivery')
    raise "terminal falsely reported app-text verification: #{delivery}" unless delivery['status'] == 'unverified'
    raise 'terminal compositor not acknowledged' unless delivery['compositor_acknowledged']
    assert_success(call('keyboard_key', key: 'Return'))
    destination = File.join(@artifacts, 'terminal-proof.txt')
    assert_success(call('export_file', session_path: "#{@home}/km-input-terminal-proof", host_path: destination))
    raise 'terminal command was truncated or changed' unless File.read(destination) == 'terminal-paced-proof'
    screen('terminal-after-return')
  end

  def partial_failure
    # pgrep runs inside the isolated PID namespace and kills only this test's compositor.
    assert_success(call('launch_app', command: "konsole --separate --nofork -e bash -c 'sleep 8; kill -KILL $(pgrep -x kwin_wayland)'"))
    response = call('keyboard_type', text: 'x' * 550)
    raise "typing survived a dead compositor: #{response}" unless response.dig('error', 'code') == -32603
    progress = response.dig('error', 'data')
    raise "missing progress: #{response}" unless progress.is_a?(Hash)
    raise "wrong requested length: #{progress}" unless progress['requested_characters'] == 550
    submitted = progress.fetch('submitted_characters')
    attempted = progress.fetch('attempted_characters')
    raise "invalid partial progress: #{progress}" unless submitted.positive? && submitted < 550 && attempted >= submitted
    raise "partial failure implied safe retry: #{progress}" unless progress['retry_safe'] == false
    raise "partial failure claimed complete submission: #{progress}" unless progress['submission_complete'] == false
    raise 'partial failure lost retry guidance' unless response.dig('error', 'message').include?('do not retype blindly')
    File.write(File.join(@artifacts, 'partial-progress.json'), JSON.pretty_generate(response))
  end

  def stop
    if @started
      assert_success(call('session_stop'))
      @started = false
    end
    raise 'owned workdir survived stop' if @workdir && File.exist?(@workdir)
    @input.close if @input && !@input.closed?
    Timeout.timeout(30) { @process.value } if @process
    @stderr&.join
    @log&.close
  end
end

server = ENV.fetch('KWIN_MCP_E2E_SERVER')
artifacts = File.expand_path(ENV.fetch('KWIN_MCP_TEST_ARTIFACT_DIR'))
probe = nil
begin
  probe = InputDeliveryProbe.new(server, artifacts)
  modes = ARGV.empty? ? %w[electron terminal] : ARGV
  modes.each { |mode| probe.public_send(mode) }
  puts "PASS: #{modes.join(', ')}; evidence #{artifacts}"
ensure
  probe&.stop
end
