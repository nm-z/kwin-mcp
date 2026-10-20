#!/usr/bin/env ruby
# Real KWin typed punctuation under a controlled scheduler pause.
# Only the private server process created by this fixture is paused.
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'digest'
require 'shellwords'

binary, evidence, mode = ARGV
abort 'usage: typed_key_batch_e2e.rb SERVER EVIDENCE baseline|candidate' unless binary && evidence && %w[baseline candidate].include?(mode)
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

report = {mode: mode}
resumed = true
watcher = nil
begin
  initialized = receive.call(send_rpc.call('initialize', {protocolVersion: '2025-06-18', capabilities: {}, clientInfo: {name: 'typed-key-batch-e2e', version: '1'}}))
  raise initialized.inspect if initialized['error']
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
  started = tool.call('session_start', {browser_profiles: 'fresh', width: 1000, height: 700})
  session = started.fetch('session_id')
  audit = started.fetch('input_log')
  tool.call('launch_app', {command: "kdialog --title 'Punctuation delivery 472' --inputbox 'The field must contain exactly one copy of each typed character.'"})
  before = tool.call('screenshot')
  FileUtils.cp(before.fetch('path'), File.join(evidence, 'before.png'))
  expected = "-a---b__c!?##//\\..~~--Repeated-Punctuation-472"
  watcher = Thread.new do
    Timeout.timeout(20) do
      loop do
        rows = File.readlines(audit).filter_map { |line| JSON.parse(line) rescue nil }
        if rows.any? { |entry| entry['event'] == 'key' && entry['pressed'] == true && entry['character'] == '-' }
          Process.kill('STOP', process.pid)
          resumed = false
          report[:paused_after_first_press] = true
          sleep 2
          Process.kill('CONT', process.pid)
          resumed = true
          break
        end
        sleep 0.001
      end
    end
  end
  response = receive.call(send_tool.call('keyboard_type', {text: expected, speed: 4}))
  watcher.value
  report[:typing] = response
  tool.call('keyboard_key', {key: 'ctrl+a'})
  tool.call('keyboard_key', {key: 'ctrl+c'})
  observed = tool.call('clipboard_read').fetch('text')
  File.write(File.join(evidence, 'expected.txt'), expected)
  File.write(File.join(evidence, 'observed.txt'), observed)
  report[:expected_characters] = expected.length
  report[:observed_characters] = observed.length
  report[:exact_text] = observed == expected
  screen = tool.call('screenshot')
  FileUtils.cp(screen.fetch('path'), File.join(evidence, 'after-pause.png'))
  if mode == 'baseline'
    raise 'baseline did not reproduce duplicate punctuation' unless report[:paused_after_first_press] && observed != expected && observed.start_with?('--')
  else
    raise "batched typing changed text: #{report}" unless report[:exact_text]
    result = data.call(response)
    raise 'incomplete submission' unless result['submission_complete'] && result['submitted_characters'] == expected.length && result['retry_safe'] == false
    samples = ['-' * 80, 'AaZz 0123456789 !@#$%^&*()_+{}[]|:;<>,.?/~', 'xxYYzz----__////..!!??']
    report[:samples] = samples.map do |sample|
      tool.call('keyboard_key', {key: 'ctrl+a'})
      typed = tool.call('keyboard_type', {text: sample, speed: 4})
      tool.call('keyboard_key', {key: 'ctrl+a'})
      tool.call('keyboard_key', {key: 'ctrl+c'})
      copied = tool.call('clipboard_read').fetch('text')
      raise "sample mismatch: #{sample.inspect} != #{copied.inspect}" unless copied == sample
      {characters: sample.length, exact: true, delivery: typed['delivery']}
    end
  end
  FileUtils.cp(audit, File.join(evidence, 'input.jsonl'))
  report[:status] = 'passed'
  puts "typed_key_batch_#{mode}=PASS"
ensure
  Process.kill('CONT', process.pid) rescue Errno::ESRCH unless resumed
  watcher.join(3) if watcher
  File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(report))
  tool.call('session_stop') if session
  input.close rescue nil
  Timeout.timeout(15) { process.value } rescue (Process.kill('TERM', process.pid) rescue Errno::ESRCH)
  error_reader.join(5)
  trace.close
end
