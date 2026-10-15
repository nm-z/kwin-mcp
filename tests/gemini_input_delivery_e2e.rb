#!/usr/bin/env ruby
# Opt-in real-site regression for #378. Uses the signed-in Chrome profile only
# inside an owned isolated desktop. Types synthetic text once, without submitting.
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'

class GeminiInputDeliveryProbe
  TEXT = 'Reply exactly: AM23_FLASH_HIGH_OK'.freeze

  def initialize(server, artifacts)
    @artifacts = artifacts
    FileUtils.mkdir_p(artifacts)
    @log = File.open(File.join(artifacts, 'rpc.jsonl'), 'w', 0o600)
    @id = 0
    @input, @output, errors, @process = Open3.popen3(
      { 'KWIN_MCP_REPO' => File.join(artifacts, 'no-source'), 'CARGO' => '/usr/bin/false' },
      server, '--width', '1280', '--height', '900', '--autoclean'
    )
    @stderr = Thread.new do
      File.open(File.join(artifacts, 'server.stderr.log'), 'w', 0o600) do |file|
        errors.each_line { |line| file.write(line); file.flush }
      end
    end
  end

  def rpc(method, params)
    @id += 1
    @input.puts(JSON.generate(jsonrpc: '2.0', id: @id, method: method, params: params))
    @input.flush
    response = Timeout.timeout(180) do
      loop do
        line = @output.gets or raise 'server closed stdout'
        message = JSON.parse(line)
        break message if message['id'] == @id
      end
    end
    # Screenshots carry an inline image; retain the file and metadata instead.
    recorded = Marshal.load(Marshal.dump(response))
    recorded.dig('result', 'content')&.reject! { |block| block['type'] == 'image' }
    @log.puts(JSON.generate(method: method, params: params, response: recorded))
    @log.flush
    response
  end

  def call(name, arguments = {})
    arguments = arguments.merge(session_id: @session_id) if @session_id
    rpc('tools/call', name: name, arguments: arguments)
  end

  def success(response)
    raise "MCP failure: #{response}" if response['error'] || response.dig('result', 'isError')
    response.dig('result', 'structuredContent')
  end

  def elements(query)
    data = success(call('find_ui_elements', query: query))
    data.fetch('results', '')
  end

  def wait_for(query, pattern)
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 60
    loop do
      match = elements(query).lines.find { |line| pattern.match?(line) }
      return match if match
      raise "missing Gemini control: #{query} / #{pattern}" if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
      sleep 0.5
    end
  end

  def click(line)
    match = line.match(/\((-?\d+), (-?\d+), (\d+)x(\d+)\)/) or raise "missing bounds: #{line}"
    x, y, width, height = match.captures.map(&:to_i)
    raise "control is off screen: #{line}" unless x >= 0 && y >= 0 && width.positive? && height.positive?
    success(call('mouse_click', x: x + width / 2, y: y + height / 2))
  end

  def run
    rpc('initialize', protocolVersion: '2024-11-05', capabilities: {},
        clientInfo: { name: 'gemini-input-delivery-e2e', version: '1' })
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
    @input.flush
    start = success(call('session_start', width: 1280, height: 900,
                         memory_high: 2, memory_max: 4, memory_swap_max: 2))
    @session_id = start.fetch('session_id')
    success(call('launch_app', command: 'google-chrome --no-first-run --no-default-browser-check --new-window https://gemini.google.com/app'))
    picker = wait_for('Open mode picker', /^button\tOpen mode picker, currently /)
    unless picker.include?('currently Flash High')
      click(picker)
      click(wait_for('Flash', /^menu item\t[^\t]*Flash All-around help\t/))
      wait_for('Open mode picker', /^button\tOpen mode picker, currently Flash High\t/)
    end
    unless elements('Temporary chats').include?("heading\tJust stopping by?")
      click(wait_for('Temporary chat', /^button\tTemporary chat\t/))
    end
    wait_for('Temporary chats', /^heading\tJust stopping by\?/)
    field = wait_for('Enter a prompt for Gemini', /^entry\tEnter a prompt for Gemini\t/)
    click(field)
    success(call('keyboard_key', key: 'ctrl+a'))
    # Keep this separate from Return and never replay it, even on failure.
    response = call('keyboard_type', text: TEXT)
    receipt = response.dig('result', 'structuredContent')
    File.write(File.join(@artifacts, 'receipt.json'), JSON.pretty_generate(response))
    visible = wait_for(TEXT, /^static\t#{Regexp.escape(TEXT)}\t/)
    File.write(File.join(@artifacts, 'independent-field.txt'), visible)
    bounds = field.match(/\((-?\d+), (-?\d+), (\d+)x(\d+)\)/).captures.map(&:to_i)
    x, y, width, height = bounds
    screen = success(call('screenshot', region: [x, y, x + width, y + height]))
    FileUtils.cp(screen.fetch('path'), File.join(@artifacts, 'prompt.png'))
    # Assert only after inspecting independent page evidence, including on the
    # old false-negative path. A successful key submission alone cannot pass.
    success(response)
    raise "incomplete submission: #{receipt}" unless receipt['submission_complete'] && receipt['submitted_characters'] == TEXT.length
    raise "unsafe retry advice: #{receipt}" unless receipt['retry_safe'] == false
    delivery = receipt.fetch('delivery')
    raise "missing compositor acknowledgement: #{delivery}" unless delivery['compositor_acknowledged']
    raise "Gemini rich text was not verified: #{delivery}" unless delivery['status'] == 'verified' && delivery['method'] == 'atspi_rich_text'
    raise "wrong expected length: #{delivery}" unless delivery['expected_field_characters'] == TEXT.length
    raise "fixture no longer reproduces the one-character focused node: #{delivery}" unless delivery['observed_field_characters'] == 1
    puts "PASS: Gemini Flash High temporary composer; verified/atspi_rich_text; expected=#{TEXT.length}, observed=1; no submission; evidence #{@artifacts}"
  end

  def stop
    success(call('session_stop')) if @session_id
  ensure
    @input&.close
    Timeout.timeout(30) { @process.value } if @process
    @stderr&.join
    @log&.close
  end
end

probe = GeminiInputDeliveryProbe.new(ENV.fetch('KWIN_MCP_E2E_SERVER'),
                                     File.expand_path(ENV.fetch('KWIN_MCP_TEST_ARTIFACT_DIR')))
begin
  probe.run
ensure
  probe.stop
end
