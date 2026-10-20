#!/usr/bin/env ruby
# Exercise real KWin focus changes during a production stdio screenshot.
# ruby tests/screenshot_target_identity_e2e.rb SERVER EVIDENCE baseline|candidate
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'digest'

binary, evidence, mode = ARGV
abort 'usage: screenshot_target_identity_e2e.rb SERVER EVIDENCE baseline|candidate' unless binary && evidence && %w[baseline candidate].include?(mode)
evidence = File.expand_path(evidence)
home = File.join(evidence, 'home')
%w[.config .cache .local/state .local/share .kde].each { |dir| FileUtils.mkdir_p(File.join(home, dir)) }
runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.uid}")
env = {
  'HOME' => home, 'XDG_CONFIG_HOME' => File.join(home, '.config'),
  'XDG_CACHE_HOME' => File.join(home, '.cache'), 'XDG_DATA_HOME' => File.join(home, '.local/share'),
  'XDG_STATE_HOME' => File.join(home, '.local/state'), 'KDEHOME' => File.join(home, '.kde'),
  'KWIN_MCP_REPO' => File.join(home, 'no-repository'), 'CARGO' => '/usr/bin/false',
  'XDG_RUNTIME_DIR' => runtime,
  'DBUS_SESSION_BUS_ADDRESS' => ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime}/bus")
}
input, output, errors, process = Open3.popen3(env, File.realpath(binary), '--autoclean', '--width', '1000', '--height', '700')
error_reader = Thread.new { File.open(File.join(evidence, 'server.log'), 'w') { |file| IO.copy_stream(errors, file) } }
trace = File.open(File.join(evidence, 'protocol.jsonl'), 'w')
sequence = 0
pending = {}
session = nil
send_rpc = lambda do |method, params|
  sequence += 1
  input.puts(JSON.generate(jsonrpc: '2.0', id: sequence, method: method, params: params))
  input.flush
  sequence
end
receive = lambda do |id|
  next pending.delete(id) if pending.key?(id)
  Timeout.timeout(60) do
    loop do
      line = output.gets or raise 'server closed stdout'
      reply = JSON.parse(line)
      trace.puts(JSON.generate(reply)); trace.flush
      break reply if reply['id'] == id
      pending[reply['id']] = reply if reply.key?('id')
    end
  end
end
send_tool = lambda do |name, args = {}|
  args = args.merge(session_id: session) if session
  send_rpc.call('tools/call', {name: name, arguments: args})
end
data = lambda do |reply|
  raise "tool failed: #{reply}" if reply['error'] || reply.dig('result', 'isError')
  reply.fetch('result').fetch('structuredContent')
end
tool = lambda { |name, args = {}| data.call(receive.call(send_tool.call(name, args))) }
frames = []
verified_race = nil
begin
  initialized = receive.call(send_rpc.call('initialize', {protocolVersion: '2025-06-18', capabilities: {}, clientInfo: {name: 'screenshot-target-identity-e2e', version: '1'}}))
  raise initialized.inspect if initialized['error']
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
  started = tool.call('session_start', {browser_profiles: 'fresh', width: 1000, height: 700})
  session = started.fetch('session_id')
  tool.call('launch_app', {command: "kdialog --title Capture-A --msgbox 'WINDOW A: original capture target'"})
  a = tool.call('window_list').fetch('windows').find { |window| window['title'].include?('Capture-A') }
  raise 'missing window A' unless a
  tool.call('launch_app', {command: "kdialog --title Capture-B --msgbox 'WINDOW B: replacement capture target'"})
  b = tool.call('window_list').fetch('windows').find { |window| window['title'].include?('Capture-B') }
  raise 'missing window B' unless b && b['id'] != a['id']

  tool.call('window_activate', {window_id: a['id']})
  stable = tool.call('screenshot')
  raise 'stable capture has wrong target' unless stable.dig('window', 'id') == a['id']
  FileUtils.cp(stable.fetch('path'), File.join(evidence, 'stable-a.png'))
  if mode == 'candidate'
    raise 'stable capture lacks identity observation' unless stable.dig('target_verification', 'method') == 'active_window_before_and_after' && stable.dig('target_verification', 'matched') && stable.dig('target_verification', 'after', 'id') == a['id']
  end

  # Activation marks recent input. The capture must observe a quiet 200 ms,
  # allowing a second real MCP request to activate B while it awaits frames.
  [0.05, 0.10, 0.15].each_with_index do |delay, attempt|
    tool.call('window_activate', {window_id: a['id']})
    previous_hash = Digest::SHA256.file(stable.fetch('path')).hexdigest
    capture_request = send_tool.call('screenshot', {inline: attempt.odd?})
    sleep delay
    activation_request = send_tool.call('window_activate', {window_id: b['id']})
    capture = receive.call(capture_request)
    activated = data.call(receive.call(activation_request))
    raise 'replacement did not activate' unless activated.dig('window', 'id') == b['id']
    current = tool.call('window_list').fetch('windows').find { |window| window['active'] }
    raise 'compositor did not retain B' unless current && current['id'] == b['id']
    frames << {delay: delay, capture: capture, active: current['id']}
    if mode == 'candidate'
      failure = capture.dig('error', 'data')
      if failure && failure['reason'] == 'capture_target_changed'
        raise capture.inspect unless failure['retry_safe'] && failure.dig('expected_window', 'id') == a['id'] && failure.dig('observed_window', 'id') == b['id']
        raise 'rejected capture published result content' if capture.key?('result')
        raise 'rejected capture overwrote the saved image' unless Digest::SHA256.file(stable.fetch('path')).hexdigest == previous_hash
        verified_race = capture
        break
      end
    else
      result = capture['result']
      info = result && result['structuredContent']
      if info && info.dig('window', 'id') == a['id']
        FileUtils.cp(info.fetch('path'), File.join(evidence, 'misattributed.png'))
        verified_race = capture
        break
      end
    end
  end
  raise "did not observe the required focus race: #{frames}" unless verified_race
  recovered = tool.call('screenshot', {region: [0, 0, 500, 350]})
  raise 'recovery crop failed' unless recovered.dig('window', 'id') == b['id'] && recovered['width'] == 500 && recovered['height'] == 350
  FileUtils.cp(recovered.fetch('path'), File.join(evidence, 'recovered-b-crop.png'))
  full = tool.call('screenshot')
  FileUtils.cp(full.fetch('path'), File.join(evidence, 'stable-b.png'))
  if mode == 'candidate'
    raise 'recovery capture lost verification metadata' unless full.dig('target_verification', 'after', 'id') == b['id']
  end
  File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(status: 'passed', mode: mode, window_a: a['id'], window_b: b['id'], stable: stable, race: verified_race, recovered: recovered, attempts: frames))
  puts "screenshot_target_identity_#{mode}=PASS"
ensure
  tool.call('session_stop') if session
  input.close rescue nil
  Timeout.timeout(15) { process.value } rescue (Process.kill('TERM', process.pid) rescue Errno::ESRCH)
  error_reader.join(5)
  trace.close
end
