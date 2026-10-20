#!/usr/bin/env ruby
# A Qt line edit that implements EditableText without the Editable state
# (Okular's toolbar page number, #580) still gets a verified typing receipt.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: qt_line_edit_receipt_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)

Dir.mktmpdir('k580-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
  FileUtils.cp('/usr/share/cups/data/form_english.pdf', File.join(home, 'doc.pdf'))
  runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.uid}")
  env = {
    'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config", 'XDG_CACHE_HOME' => "#{home}/.cache",
    'XDG_DATA_HOME' => "#{home}/.local/share", 'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
    'XDG_RUNTIME_DIR' => runtime,
    'DBUS_SESSION_BUS_ADDRESS' => ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime}/bus"),
    'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil, 'XAUTHORITY' => nil,
    'KWIN_MCP_REPO' => "#{home}/no-source", 'CARGO' => '/usr/bin/false'
  }
  trace = File.open(File.join(evidence, 'rpc.jsonl'), 'w')
  input, output, process = Open3.popen2(env, binary, '--autoclean', err: File.join(evidence, 'server.stderr'))
  id = 0
  rpc = lambda do |method, params|
    id += 1
    request = {jsonrpc: '2.0', id: id, method: method, params: params}
    trace.puts(JSON.generate(request: request)); trace.flush
    input.puts(JSON.generate(request)); input.flush
    response = Timeout.timeout(90) { loop { value = JSON.parse(output.gets || raise('server EOF')); break value if value['id'] == id } }
    trace.puts(JSON.generate(response: response)); trace.flush
    response
  end
  session = nil
  data = lambda do |name, args = {}|
    response = rpc.call('tools/call', {name: name, arguments: session ? args.merge(session_id: session) : args})
    raise "#{name} failed: #{response}" if response['error'] || response.dig('result', 'isError')
    response.dig('result', 'structuredContent')
  end
  begin
    rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'qt-line-edit-receipt', version: '1'}})
    input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
    session = data.call('session_start', {width: 1280, height: 800, browser_profiles: 'fresh'}).fetch('session_id')
    data.call('launch_app', {command: "okular #{home}/doc.pdf", window_timeout_seconds: 30})
    sleep 2
    # The page-number field is the role "text" node right of the previous-page button.
    field = data.call('find_ui_elements', {query: 'of', role: 'label'}).fetch('elements', []).find { |element| element['name'] == 'of' } ||
            raise('no "of" label beside the page-number field')
    bounds = field.fetch('bounds')
    data.call('mouse_click', {x: (bounds['x'] - 20).to_i, y: (bounds['y'] + bounds['h'] / 2).to_i, triple: true})
    data.call('keyboard_key', {key: 'ctrl+a'})
    typed = data.call('keyboard_type', {text: '1'})
    delivery = typed.fetch('delivery')
    File.write(File.join(evidence, 'receipt.json'), JSON.pretty_generate(typed))
    raise "receipt not verified: #{delivery}" unless delivery['status'] == 'verified' && delivery['method'] == 'atspi_focused_text'
    puts "PASS: #{JSON.generate(delivery)[0, 300]}"
  ensure
    data.call('session_stop') if session rescue nil
    input.close
    Timeout.timeout(30) { process.value }
    trace.close
  end
end
