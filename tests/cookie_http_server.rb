#!/usr/bin/env ruby
require 'json'
require 'open3'
require 'socket'

port = Integer(ARGV.fetch(0))
log_path = ARGV.fetch(1)
expected = ARGV.fetch(2)
home = ENV.fetch('HOME')
cookie_counts = Dir.glob(File.join(home, '.config/google-chrome/Default/**/Cookies')).filter_map do |path|
  output, status = Open3.capture2(
    'sqlite3', path,
    "SELECT count(*) FROM cookies WHERE name IN ('kwin_immediate','kwin_session')"
  )
  { path: path.delete_prefix(home), rows: Integer(output.strip) } if status.success?
end
File.write(File.join(home, 'cookie-counts.json'), JSON.generate(cookie_counts))
server = TCPServer.new('127.0.0.1', port)
File.write(File.join(home, 'cookie-http-ready'), "ready\n")

# The parent becomes Konsole so launch_app has a visible window to report.
# The child owns the isolated session's loopback HTTP endpoint.
if Process.fork
  server.close
  exec('konsole')
end

loop do
  connection = server.accept
  begin
    File.open(File.join(home, 'cookie-http-server.log'), 'a') do |log|
      log.write("accepted\n")
      log.flush
    end
    request = connection.gets.to_s
    headers = {}
    while (line = connection.gets) && line != "\r\n"
      name, value = line.split(':', 2)
      headers[name.to_s.downcase] = value.to_s.strip
    end
    route = request.split[1].to_s
    cookies = headers.fetch('cookie', '').split(';').map(&:strip).to_h do |cookie|
      name, value = cookie.split('=', 2)
      [name, value]
    end
    immediate = expected == '-' ? cookies.key?('kwin_immediate') : cookies['kwin_immediate'] == expected
    session = expected == '-' ? cookies.key?('kwin_session') : cookies['kwin_session'] == expected
    setting = route.start_with?('/set/')
    value = setting ? route.delete_prefix('/set/') : ''
    title = setting ? "SET #{value}" : (immediate && session ? 'COOKIE PRESENT' : 'COOKIE MISSING')
    body = "<!doctype html><html><title>Cookie continuity</title><body style='font:32px sans-serif'><h1>#{title}</h1><p>Private real Chrome cookie persistence test</p></body></html>"
    set_cookies = setting ? "Set-Cookie: kwin_immediate=#{value}; Max-Age=3600; Path=/; HttpOnly\r\nSet-Cookie: kwin_session=#{value}; Path=/; HttpOnly\r\n" : ''
    connection.write("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: #{body.bytesize}\r\n#{set_cookies}Connection: close\r\n\r\n#{body}")
    File.open(log_path, 'a') do |log|
      log.write(JSON.generate(route: route, immediate: immediate, session: session))
      log.write("\n")
      log.flush
      log.fsync
    end
  rescue StandardError => error
    File.open(File.join(home, 'cookie-http-server.log'), 'a') do |log|
      log.write("#{error.class}: #{error.message}\n")
      log.flush
    end
  ensure
    connection.close
  end
end
