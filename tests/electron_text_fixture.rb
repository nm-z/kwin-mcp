# A static, synthetic document in an owned Electron renderer. No JavaScript is
# authored or evaluated. See Electron remote-debugging-port and CDP Page APIs.
require 'socket'
require 'net/http'
require 'uri'
require 'base64'
require 'digest'
require 'json'
require 'timeout'

module ElectronTextFixture
  HTML = <<~HTML.freeze
    <!doctype html><html><head><title>Input delivery fixture</title>
    <style>body{font:20px sans-serif;padding:28px}.entry{display:block;width:90%;height:260px;font:18px monospace;white-space:pre-wrap;border:2px solid;padding:12px}.entry.rich{height:130px}.entry.nested{height:110px}.embedded-editor{display:block;width:100%;height:80px;padding:8px;border:1px solid #888}.opaque-object{display:block;width:100%;height:16px;background:#ddd}.protected-entry{display:block;width:90%;height:44px;font:18px monospace;border:2px solid;padding:8px}</style>
    </head><body><h1>Private Electron input delivery</h1>
    <label>Do anything</label><div class="entry" role="textbox" aria-label="Do anything" contenteditable="true" tabindex="0" autofocus spellcheck="false"></div>
    <label>Rich contenteditable</label><div class="entry rich" role="textbox" aria-label="Rich contenteditable" contenteditable="true" tabindex="0" spellcheck="false"><span style="font-weight:700">seed </span><span style="font-style:italic">text</span></div>
    <!-- The focused parent exposes an embedded-object character; child text exposes the value. -->
    <label>Nested rich contenteditable</label><div class="entry rich nested" role="textbox" aria-label="Nested rich contenteditable" contenteditable="true" tabindex="0" spellcheck="false"><div class="embedded-editor" role="textbox" aria-label="Embedded rich text" contenteditable="true" tabindex="0"><span style="font-weight:700">seed </span><span style="font-style:italic">text</span></div></div>
    <label>Unverified password field</label><input class="protected-entry" type="password" aria-label="Unverified password field" autocomplete="off" tabindex="0">
    <!-- The embedded editor accepts input but is hidden from AT-SPI; the graphic keeps the parent at U+FFFC. -->
    <label>Unverified embedded rich text</label><div class="entry rich" role="textbox" aria-label="Unverified embedded rich text" contenteditable="true" tabindex="0" spellcheck="false"><div class="embedded-editor" contenteditable="true" tabindex="0" aria-hidden="true"><span style="font-weight:700">seed </span><span style="font-style:italic">text</span></div><span class="opaque-object" role="img" aria-label="Opaque embedded object" contenteditable="false"></span></div>
    </body></html>
  HTML

  def self.free_port
    socket=TCPServer.new('127.0.0.1',0); port=socket.addr[1];socket.close;port
  end

  def self.install(port)
    target=nil
    Timeout.timeout(30) do
      loop do
        begin
          client=Net::HTTP.new('127.0.0.1',port,nil);client.open_timeout=1;client.read_timeout=1
          list=JSON.parse(client.get('/json/list').body)
          target=list.find {|entry|entry['type']=='page' && entry['title']=='ChatGPT'}
          break if target
        rescue SystemCallError, IOError, Net::ReadTimeout, Net::OpenTimeout, JSON::ParserError
        end
        sleep 0.1
      end
    end
    uri=URI(target.fetch('webSocketDebuggerUrl'))
    raise 'unexpected debugger endpoint' unless uri.scheme=='ws' && %w[127.0.0.1 localhost].include?(uri.host) && uri.port==port
    socket=TCPSocket.new(uri.host,uri.port)
    nonce=Base64.strict_encode64(Random.bytes(16))
    socket.write("GET #{uri.request_uri} HTTP/1.1\r\nHost: #{uri.host}:#{uri.port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: #{nonce}\r\nSec-WebSocket-Version: 13\r\n\r\n")
    headers=Timeout.timeout(5) {socket.gets("\r\n\r\n")}
    accept=Base64.strict_encode64(Digest::SHA1.digest(nonce+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11'))
    raise 'debugger handshake rejected' unless headers && headers.start_with?('HTTP/1.1 101') && headers.include?(accept)
    send_frame=lambda do |opcode,payload|
      bytes=payload.b; mask=Random.bytes(4).bytes; length=bytes.bytesize
      prefix=length<126 ? [0x80|opcode,0x80|length].pack('CC') : [0x80|opcode,0x80|126,length].pack('CCn')
      socket.write(prefix+mask.pack('C*')+bytes.bytes.each_with_index.map {|b,index|b^mask[index%4]}.pack('C*'))
    end
    exact=lambda do |length|
      buffer=''.b
      while buffer.bytesize<length
        part=socket.read(length-buffer.bytesize) or raise 'debugger EOF'
        buffer << part
      end
      buffer
    end
    sequence=0
    command=lambda do |method,params|
      sequence+=1;send_frame.call(1,JSON.generate(id:sequence,method:method,params:params))
      Timeout.timeout(10) do
        loop do
          first,second=exact.call(2).unpack('CC');opcode=first&15;length=second&127
          length=exact.call(2).unpack1('n') if length==126
          length=exact.call(8).unpack1('Q>') if length==127
          raise 'oversized debugger frame' if length>16*1024*1024
          raise 'masked debugger server frame' unless second&128==0
          payload=exact.call(length)
          if opcode==9;send_frame.call(10,payload);next;end
          raise 'debugger closed' if opcode==8
          next unless opcode==1
          message=JSON.parse(payload)
          next unless message['id']==sequence
          raise message.inspect if message['error']
          break message.fetch('result')
        end
      end
    end
    tree=command.call('Page.getFrameTree',{})
    frame=tree.fetch('frameTree').fetch('frame').fetch('id')
    command.call('Page.setDocumentContent',{frameId:frame,html:HTML})
    {port:port,target:target['id'],frame:frame,method:'Page.setDocumentContent',synthetic:true}
  ensure
    socket&.close
  end
end

if $PROGRAM_NAME == __FILE__
  port=Integer(ARGV.fetch(0))
  raise 'invalid debugger port' unless (1..65535).cover?(port)
  puts JSON.generate(ElectronTextFixture.install(port))
end
