#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'timeout'

report_path = ARGV.fetch(0)
normal_out, normal_err, normal_status = Open3.capture3('pkttyagent', '--process', Process.pid.to_s)
raise "agent exit changed: #{normal_status.exitstatus}" unless normal_status.exitstatus == 126
raise "registration diagnostic reached terminal: #{normal_err}" unless normal_err.empty?

_, invalid_err, invalid_status = Open3.capture3('pkttyagent', '--not-a-real-option')
raise 'unrelated error was hidden' if invalid_err.empty? || invalid_status.success?

reader, writer = IO.pipe
notify_error = report_path + '.notify.stderr'
notify_pid = Process.spawn('pkttyagent', '--process', Process.pid.to_s, '--notify-fd', '9',
  9 => writer, out: File::NULL, err: notify_error)
writer.close
notify_eof = Timeout.timeout(4) { reader.read }
reader.close
_, notify_status = Process.wait2(notify_pid)
raise 'notify fd did not close cleanly' unless notify_eof.empty? && notify_status.exitstatus == 126

_, fallback_err, fallback_status = Open3.capture3(
  {'XDG_RUNTIME_DIR' => '/run/kwin-mcp-missing-log-directory'},
  'pkttyagent', '--process', Process.pid.to_s)
raise 'failed logging swallowed diagnostic' unless fallback_err.include?('Error registering authentication agent:') && fallback_status.exitstatus == 126

log_path = File.join(ENV.fetch('XDG_RUNTIME_DIR'), 'polkit-agent.log')
log = File.read(log_path)
raise 'registration diagnostic missing from log' unless log.lines.count { |line| line.start_with?('Error registering authentication agent:') } >= 2
payload = {status: 'PASS', normal_exit: normal_status.exitstatus, normal_stderr: normal_err,
  unrelated_error_visible: true, notify_fd_closed: true, logging_failure_visible: true,
  diagnostic_lines: log.lines.size}
File.write(report_path + '.tmp', JSON.pretty_generate(payload))
File.rename(report_path + '.tmp', report_path)
puts 'POLKIT DIAGNOSTIC REGRESSION: PASS'
puts 'Normal terminal output remains visible.'
