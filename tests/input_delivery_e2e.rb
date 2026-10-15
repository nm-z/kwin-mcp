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

  def launch_electron_fixture
    debug_port = ElectronTextFixture.free_port
    command = '/usr/lib/chatgpt/ChatGPT --user-data-dir="$HOME/.km-input-e2e-private" --force-renderer-accessibility' + " --remote-debugging-port=#{debug_port}"
    assert_success(call('launch_app', command: command))
    helper = File.expand_path('electron_text_fixture.rb', __dir__)
    command = "ruby '#{helper}' #{debug_port} > \"$HOME/electron-document.json\" 2> \"$HOME/electron-document.stderr\""
    response = call('launch_app', command: command)
    assert_success(response)
    destination = File.join(@artifacts, 'electron-document.json')
    assert_success(call('export_file', session_path: "#{@home}/electron-document.json", host_path: destination))
    fixture = JSON.parse(File.read(destination))
    raise 'non-synthetic document' unless fixture['synthetic']
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 60
    visible = nil
    loop do
      visible = tree
      break if visible.lines.any? { |line| line.include?("entry\tDo anything\t") }
      raise 'Electron composer did not become accessible' if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
      sleep 0.5
    end
    visible
  end

  def electron
    visible = launch_electron_fixture
    # The composer starts focused; an explicit click verifies active-window routing.
    match = visible.match(/entry\tDo anything\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'composer bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    [550, 250].each do |length|
      # Replace the complete selection. Chromium turns a cleared contenteditable
      # into a synthetic newline; deleting first would test that transformation
      # rather than the exact insertion/selection receipt.
      assert_success(call('keyboard_key', key: 'ctrl+a'))
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
    stale_descendant_rich_text
    rich_contenteditable
    nested_rich_contenteditable
    unverified_password_field
    unverified_embedded_rich_text
  end

  # Fast coverage for #376: initial text/selection, verified embedded text, and
  # an opaque one-character rich editor whose contents cannot be verified.
  def context
    launch_electron_fixture
    focus_entry('Do anything', 'context-empty')
    text = 'Reply with only the number: 17*23'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise "empty composer was not verified: #{delivery}" unless delivery['status'] == 'verified'
    assert_field_context(delivery, initial: [0, 0, 0], range: [0, 0], observed: [33, 0, 0])
    raise 'independent tree lacks composer text' unless tree.include?(text)

    # Select five ASCII characters in the middle, preserving both surrounding parts.
    assert_success(call('keyboard_key', key: 'Home'))
    5.times { assert_success(call('keyboard_key', key: 'Right')) }
    5.times { assert_success(call('keyboard_key', key: 'shift+Right')) }
    replacement = 'selection'
    result = assert_success(call('keyboard_type', text: replacement))
    delivery = result.fetch('delivery')
    expected = text[0, 5] + replacement + text[10..]
    raise "selection replacement was not verified: #{delivery}" unless delivery['status'] == 'verified'
    assert_field_context(delivery, initial: [33, 0, 0], range: [5, 10], observed: [expected.length, 0, 0])
    raise 'independent tree lacks replaced composer text' unless tree.include?(expected)

    # A cleared Chromium contenteditable exposes a newline before input.
    assert_success(call('keyboard_key', key: 'ctrl+a'))
    assert_success(call('keyboard_key', key: 'Backspace'))
    response = call('keyboard_type', text: text)
    raise "unexpected protocol error: #{response}" if response['error']
    delivery = response.fetch('result').fetch('structuredContent').fetch('delivery')
    File.write(File.join(@artifacts, 'cleared-composer.json'), JSON.pretty_generate(response))
    raise "cleared composer did not preserve mismatch: #{delivery}" unless delivery['status'] == 'pending_or_mismatched' && response.dig('result', 'isError')
    raise "wrong cleared expected count: #{delivery}" unless delivery['expected_field_characters'] == 34
    assert_field_context(delivery, initial: [1, 0, 1], range: [0, 0], observed: [33, 0, 0])
    result = response.fetch('result').fetch('structuredContent')
    raise 'cleared composer suggested blind retry' unless result['retry_safe'] == false && result['submission_complete']
    raise 'cleared composer lacks independent complete text' unless tree.include?(text)

    nested_rich_contenteditable
    unverified_embedded_rich_text
    unverified_password_field
  end

  def focus_entry(label, tag)
    match = tree.match(/entry\t#{Regexp.escape(label)}\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise "entry bounds missing: #{label}" unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen(tag).fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
  end

  def assert_field_context(delivery, initial:, range:, observed:)
    field = delivery.fetch('field')
    raise "wrong field role: #{field}" unless field['role'] == 'Entry'
    raise "wrong insertion range: #{field}" unless field.values_at('insertion_start', 'insertion_end') == range
    keys = %w[characters embedded_object_characters line_break_characters]
    raise "wrong initial snapshot: #{field}" unless field.fetch('initial').values_at(*keys) == initial
    raise "wrong observed snapshot: #{field}" unless field.fetch('observed').values_at(*keys) == observed
    # The receipt ID is usable with the public accessibility tool to inspect the target.
    response = assert_success(call('accessibility_tree', root: field.fetch('node_id')))
    raise "field ID returned no accessible nodes: #{response}" unless response.fetch('nodes').positive?
  end

  def rich_contenteditable
    visible = tree
    match = visible.match(/entry\tRich contenteditable\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'rich contenteditable bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-rich-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    assert_success(call('keyboard_key', key: 'ctrl+a'))
    text = 'Rich contenteditable delivery probe: alpha beta 12345.'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise 'rich contenteditable omitted completed submission' unless result['submission_complete']
    raise 'rich contenteditable lacks compositor acknowledgement' unless delivery['compositor_acknowledged']
    raise "rich contenteditable suggested retry: #{delivery}" unless result['retry_safe'] == false
    raise "wrong rich contenteditable status: #{delivery}" unless delivery['status'] == 'verified'
    raise "wrong rich contenteditable method: #{delivery}" unless delivery['method'] == 'atspi_focused_text'
    raise "wrong rich-text expected length: #{delivery}" unless delivery['expected_field_characters'] == text.length
    raise "wrong rich-text observed length: #{delivery}" unless delivery['observed_field_characters'] == text.length
    visible = tree
    File.write(File.join(@artifacts, 'electron-rich-contenteditable.tree.txt'), visible)
    raise 'rich contenteditable lacks complete text after acknowledged input' unless visible.include?(text)
    screen('electron-rich-after-return')
  end

  def nested_rich_contenteditable
    visible = tree
    match = visible.match(/entry\tNested rich contenteditable\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'nested rich contenteditable bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-nested-rich-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    assert_success(call('keyboard_key', key: 'ctrl+a'))
    text = 'Nested rich contenteditable probe: alpha beta 12345.'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise 'nested rich contenteditable omitted completed submission' unless result['submission_complete']
    raise 'nested rich contenteditable lacks compositor acknowledgement' unless delivery['compositor_acknowledged']
    raise "nested rich contenteditable suggested retry: #{delivery}" unless result['retry_safe'] == false
    raise "nested rich contenteditable was not verified: #{delivery}" unless delivery['status'] == 'verified'
    raise "nested rich contenteditable used the wrong method: #{delivery}" unless delivery['method'] == 'atspi_rich_text'
    raise "wrong nested rich expected length: #{delivery}" unless delivery['expected_field_characters'] == text.length
    raise "nested rich focused node was not the single embedded-object character: #{delivery}" unless delivery['observed_field_characters'] == 1
    assert_field_context(delivery, initial: [1, 1, 0], range: [0, 1], observed: [1, 1, 0])
    visible = tree
    File.write(File.join(@artifacts, 'electron-nested-rich-contenteditable.tree.txt'), visible)
    raise 'nested rich contenteditable accessibility tree lacks complete text' unless visible.include?(text)
    screen('electron-nested-rich-after-return')
  end

  def unverified_embedded_rich_text
    visible = tree
    match = visible.match(/entry\tUnverified embedded rich text\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'unverified embedded rich-text bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-unverified-embedded-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    text = 'Unverified embedded delivery probe'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise 'embedded rich text omitted completed submission' unless result['submission_complete']
    raise 'embedded rich text lacks compositor acknowledgement' unless delivery['compositor_acknowledged']
    raise "embedded rich text suggested retry: #{delivery}" unless result['retry_safe'] == false
    raise "embedded rich text did not report unverified: #{delivery}" unless delivery['status'] == 'unverified'
    raise "embedded rich text used the wrong method: #{delivery}" unless delivery['method'] == 'atspi_rich_text'
    raise "wrong embedded rich-text expected length: #{delivery}" unless delivery['expected_field_characters'] == text.length
    raise "embedded rich-text focused node was not the single object character: #{delivery}" unless delivery['observed_field_characters'] == 1
    field = delivery.fetch('field')
    raise "opaque field did not identify the embedded object: #{field}" unless field.fetch('observed').values_at('characters', 'embedded_object_characters', 'line_break_characters') == [1, 1, 0]
    raise 'opaque field omitted initial snapshot or insertion range' unless field['initial'] && field['insertion_start'] && field['insertion_end']
    raise 'opaque field lost retry guidance' unless delivery.fetch('warning').include?('do not retype blindly')
    visible = tree
    File.write(File.join(@artifacts, 'electron-unverified-embedded.tree.txt'), visible)
    raise 'opaque embedded object unexpectedly exposed the typed text' if visible.include?(text)
    screen('electron-unverified-embedded-after-return')
  end

  def stale_descendant_rich_text
    visible = tree
    match = visible.match(/entry\tStale descendant rich text\t[^\t]*\t\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'stale descendant rich-text bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-stale-descendant-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    text = 'Stale descendant delivery probe'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise 'stale descendant omitted completed submission' unless result['submission_complete']
    raise 'stale descendant lacks compositor acknowledgement' unless delivery['compositor_acknowledged']
    raise "stale descendant suggested retry: #{delivery}" unless result['retry_safe'] == false
    raise "stale descendant falsely reported verified: #{delivery}" if delivery['status'] == 'verified'
    raise "stale descendant did not report rich-text unverified: #{delivery}" unless delivery['status'] == 'unverified' && delivery['method'] == 'atspi_rich_text'
    raise "wrong stale descendant expected length: #{delivery}" unless delivery['expected_field_characters'] == text.length
    visible = tree
    File.write(File.join(@artifacts, 'electron-stale-descendant.tree.txt'), visible)
    raise 'stale note was not exposed in the independent accessibility tree' unless visible.include?(text)
    raise 'stale note appeared more than once in the independent accessibility tree' unless visible.scan(text).length == 1
    screen('electron-stale-descendant-after-return')
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

  def unverified_password_field
    visible = tree
    line = visible.lines.find do |candidate|
      candidate.include?("password text\tUnverified password field\t")
    end
    raise 'unverified password field is missing from the accessibility tree' unless line
    match = line.match(/\((-?\d+), (-?\d+), (\d+), (\d+)\)/)
    raise 'unverified password field bounds missing' unless match
    x, y, width, height = match.captures.map(&:to_i)
    window = screen('electron-password-before').fetch('window')
    assert_success(call('mouse_click', x: (x + width / 2 - window.fetch('x')).round,
                        y: (y + height / 2 - window.fetch('y')).round))
    text = 'Unverified delivery probe'
    result = assert_success(call('keyboard_type', text: text))
    delivery = result.fetch('delivery')
    raise 'password field omitted completed submission' unless result['submission_complete']
    raise 'password field lacks compositor acknowledgement' unless delivery['compositor_acknowledged']
    raise "password field suggested retry: #{delivery}" unless result['retry_safe'] == false
    raise "password field did not report unverified: #{delivery}" unless delivery['status'] == 'unverified'
    raise "password field used an unexpected fallback method: #{delivery}" unless delivery['method'] == 'eis_roundtrip'
    raise "password field exposed text character counts: #{delivery}" unless delivery['expected_field_characters'].nil? && delivery['observed_field_characters'].nil?
    raise 'password field exposed field details' if delivery.key?('field')
    screen('electron-password-after-return')
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
