#!/usr/bin/env ruby
# Typing after a non-editable mention chip in a contenteditable composer
# (#575): the editor's descendants hold the chip text plus the typed text, and
# the receipt must still verify the typed text.
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: rich_text_mention_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)

COMPOSER = <<~HTML
  <!doctype html><title>kwin-575 composer</title>
  <div contenteditable=true role=textbox aria-multiline=true aria-label=Composer id=c
       style="border:1px solid #888;min-height:120px;font:20px sans-serif;padding:8px"><p><span contenteditable=false
       style="background:#ccf;border-radius:4px;padding:2px">@kwin-mcp</span>&nbsp;</p></div>
  <script>
    const c = document.getElementById('c'); c.focus();
    const r = document.createRange(); r.selectNodeContents(c.firstChild); r.collapse(false);
    getSelection().removeAllRanges(); getSelection().addRange(r);
  </script>
HTML
TYPED = ' please list the open windows in my isolated desktop and describe each one briefly'

Dir.mktmpdir('k575-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
  page = File.join(home, 'composer.html')
  File.write(page, COMPOSER)
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
  session = nil
  rpc = lambda do |method, params|
    id += 1
    request = {jsonrpc: '2.0', id: id, method: method, params: params}
    trace.puts(JSON.generate(request: request)); trace.flush
    input.puts(JSON.generate(request)); input.flush
    response = Timeout.timeout(120) { loop { value = JSON.parse(output.gets || raise('server EOF')); break value if value['id'] == id } }
    trace.puts(JSON.generate(response: response)); trace.flush
    response
  end
  data = lambda do |name, args = {}|
    response = rpc.call('tools/call', {name: name, arguments: session ? args.merge(session_id: session) : args})
    raise "#{name} failed: #{response}" if response['error'] || response.dig('result', 'isError')
    response.dig('result', 'structuredContent')
  end
  begin
    rpc.call('initialize', {protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'rich-text-mention', version: '1'}})
    input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
    session = data.call('session_start', {width: 1280, height: 800, browser_profiles: 'fresh'}).fetch('session_id')
    data.call('launch_app', {command: "google-chrome-stable --no-first-run --no-default-browser-check #{Shellwords.escape("file://#{page}")}", window_timeout_seconds: 30})
    sleep 3
    typed = data.call('keyboard_type', {text: TYPED})
    delivery = typed.fetch('delivery')
    File.write(File.join(evidence, 'receipt.json'), JSON.pretty_generate(typed))
    shot = data.call('screenshot', {})
    FileUtils.cp(shot.fetch('path'), File.join(evidence, 'composer.png'))
    raise "receipt not verified: #{delivery}" unless delivery['status'] == 'verified' && delivery['method'] == 'atspi_rich_text'
    puts "PASS: #{JSON.generate(delivery)[0, 300]}"
  ensure
    begin
      data.call('session_stop') if session
    rescue StandardError
      nil
    end
    input.close
    Timeout.timeout(30) { process.value }
    trace.close
  end
end
