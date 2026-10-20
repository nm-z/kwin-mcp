require 'socket'
require 'uri'
require 'cgi'
require 'shellwords'

class NativePasteFixture
  attr_reader :port
  def initialize
    @listener = TCPServer.new('127.0.0.1', 0)
    @port = @listener.addr[1]
    @posts = Queue.new
    @workers = []
    @acceptor = Thread.new do
      loop do
        client = @listener.accept
        @workers << Thread.new(client) { |socket| serve(socket) }
      end
    rescue IOError, Errno::EBADF
    end
  end
  def post
    Timeout.timeout(15) { @posts.pop }
  end
  def close
    @listener.close
    @acceptor.join
    @workers.each { |worker| worker.join(6) }
  end
  def serve(socket)
    Timeout.timeout(5) do
      line = socket.gets
      return unless line
      method, target = line.split(' ')
      uri = URI.parse(target)
      case_id = URI.decode_www_form(uri.query.to_s).to_h.fetch('case', '')
      raise 'invalid fixture case' unless case_id.match?(/\A[a-z_]*\z/)
      headers = {}
      while (line = socket.gets) && line != "\r\n"
        name, value = line.split(':', 2)
        headers[name.downcase] = value.strip
      end
      if method == 'POST'
        size = Integer(headers.fetch('content-length', '0'))
        raise 'oversized fixture body' unless (0..65_536).cover?(size)
        body = socket.read(size)
        raise 'incomplete body' unless body && body.bytesize == size
        @posts << {case: case_id, values: URI.decode_www_form(body, Encoding::UTF_8)}
        html = '<!doctype html><meta charset="utf-8"><title>Paste received</title><h1>Submission received</h1>'
      elsif uri.path == '/protected'
        html = '<!doctype html><meta charset="utf-8"><title>Native paste protected</title><label>Protected field <input type="password" autofocus></label>'
      else
        html = <<~HTML
          <!doctype html><meta charset="utf-8"><title>Native paste #{case_id}</title>
          <style>body{margin:24px;background:#202020;color:#eee;font:18px sans-serif}textarea{display:block;width:950px;height:320px;padding:12px;font:18px monospace}button{margin-top:16px;padding:12px 24px}</style>
          <form method="post" action="/submit?case=#{case_id}" accept-charset="UTF-8">
          <label>Regression comment #{case_id}<textarea name="comment" autofocus></textarea></label>
          <button type="submit">Submit regression</button></form>
        HTML
      end
      socket.write("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: #{html.bytesize}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")
      socket.write(html)
    end
  rescue IOError, EOFError, Errno::ECONNRESET, Errno::EPIPE, Timeout::Error
  ensure
    socket.close unless socket.closed?
  end
end

def native_paste_regression(call, content, started, args, evidence)
  fixture = NativePasteFixture.new
  checked = lambda do |tool, extra = {}|
    result = call.call(tool, args.merge(extra))
    raise "#{tool} failed: #{result.inspect}" if result['isError']
    result
  end
  key = lambda { |combo| checked.call('keyboard_key', {key: combo}) }
  screenshot = lambda do |label|
    result = checked.call('screenshot')
    FileUtils.cp(content.call(result).fetch('path'), File.join(evidence, "#{label}.png"))
  end
  observations = []
  begin
    [%w[ctrl_v ctrl+v], %w[shift_insert shift+insert], %w[protected ctrl+v]].each do |case_id, shortcut|
      profile = File.join(started.fetch('workdir'), "paste-profile-#{case_id}")
      path = case_id == 'protected' ? '/protected' : '/form'
      url = "http://127.0.0.1:#{fixture.port}#{path}?case=#{case_id}"
      command = ['google-chrome-stable', '--new-window', '--no-first-run',
        '--no-default-browser-check', "--user-data-dir=#{profile}", url].shelljoin
      checked.call('launch_app', {command: command, window_timeout_seconds: 30})
      Timeout.timeout(10) do
        loop do
          windows = content.call(checked.call('window_list')).fetch('windows')
          break if windows.any? { |window| window['active'] && window['title'].include?("Native paste #{case_id}") }
          sleep 0.1
        end
      end
      screenshot.call("#{case_id}-before")
      if case_id == 'protected'
        checked.call('clipboard_write', {target:'session', generate_secret:{length:20, character_classes:%w[lowercase uppercase digits symbols]}})
        result = content.call(key.call(shortcut))
        paste = result.fetch('paste')
        delivery = paste.fetch('editable_value')
        raise 'protected value was inspected' unless delivery['status'] == 'unverified' && delivery['method'] == 'paste_protected_clipboard' && delivery['expected_field_characters'].nil? && delivery['observed_field_characters'].nil?
        raise 'incorrect protected receipt' unless paste['shortcut_acknowledged'] && paste['form_submission'] == 'not_observed'
        observations << {case:case_id, paste:paste}
        checked.call('clipboard_write', {target:'session', text:'fixture complete'})
      else
        payload = "# Native paste #{case_id}\n\nA paragraph with café and Ω.\nComplete multiline content.\n"
        checked.call('clipboard_write', {target:'session', text:payload})
        result = content.call(key.call(shortcut))
        paste = result.fetch('paste')
        delivery = paste.fetch('editable_value')
        raise "unverified paste: #{paste.inspect}" unless paste['shortcut_acknowledged'] && delivery['status'] == 'verified'
        raise 'incorrect field count' unless delivery['expected_field_characters'] == payload.length && delivery['observed_field_characters'] == payload.length
        raise 'premature form claim' unless paste['form_submission'] == 'not_observed' && paste['retry_safe'] == false
        screenshot.call("#{case_id}-pasted")
        key.call('Tab')
        key.call('Return')
        post = fixture.post
        expected = payload.gsub("\n", "\r\n")
        raise 'native submitted value differs' unless post[:case] == case_id && post[:values] == [['comment', expected]]
        observations << {case:case_id, paste:paste, exact_native_form_value:true,
          submitted:expected, form_line_endings:'CRLF'}
        screenshot.call("#{case_id}-submitted")
      end
      key.call('ctrl+shift+w')
    end
    File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate(observations))
    puts 'native_paste=PASS'
  ensure
    fixture.close
  end
end
