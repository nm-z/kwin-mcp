#!/usr/bin/env ruby
# Real private D-Bus/KWin startup. Corrupt only the sandbox's generated config.
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'
require 'timeout'

server = File.realpath(ARGV.fetch(0))
evidence = File.expand_path(ARGV.fetch(1))
FileUtils.mkdir_p(evidence)
daemon = ENV.fetch('PATH').split(':').map { |part| File.join(part,'dbus-daemon') }.find { |path| File.executable?(path) }
abort 'dbus-daemon is required' unless daemon
baseline = ENV['KWIN_MCP_E2E_BASELINE'] == '1'

def request(input, output, id, method, params={})
  input.puts(JSON.generate(jsonrpc:'2.0',id:id,method:method,params:params)); input.flush
  Timeout.timeout(60) do
    loop do
      line = output.gets or raise 'server EOF'
      response = JSON.parse(line)
      return response if response['id'] == id
    end
  end
end

def data(response)
  raise response.inspect if response['error'] || response.dig('result','isError')
  response.fetch('result').fetch('structuredContent')
end

Dir.mktmpdir('kw450-', '/var/tmp') do |root|
  wrappers = File.join(root,'bin')
  runtime = File.join(root,'runtime')
  FileUtils.mkdir_p([wrappers,runtime])
  File.write(File.join(wrappers,'dbus-daemon'), <<~'BASH')
    #!/usr/bin/env bash
    set -eu
    for arg in "$@"; do
      if [[ "$arg" == --config-file=/tmp/mcp-dbus.conf && "$KW450_MODE" != normal ]]; then
        printf '<busconfig><broken/></busconfig>\n' > /tmp/mcp-dbus.conf
        if [[ "$KW450_MODE" == noisy ]]; then
          head -c 131072 /dev/zero | tr '\000' x >&2
          printf '\nkw450-tail\377\n' >&2
        fi
      fi
    done
    exec "$KW450_DAEMON" "$@"
  BASH
  File.chmod(0o755,File.join(wrappers,'dbus-daemon'))
  upstream = File.join(root,'upstream')
  upstream_log = File.open(File.join(evidence,'upstream.stderr'),'w')
  upstream_pid = Process.spawn(daemon,'--session','--nofork','--nopidfile',"--address=unix:path=#{upstream}",out:File::NULL,err:upstream_log)
  begin
    Timeout.timeout(5) { sleep 0.02 until File.socket?(upstream) }
    %w[invalid noisy normal].each do |mode|
      home = File.join(root,mode,'home')
      FileUtils.mkdir_p(%w[.config .cache .local/share .local/state .kde].map { |name| File.join(home,name) })
      env = {'HOME'=>home,'XDG_CONFIG_HOME'=>"#{home}/.config",'XDG_DATA_HOME'=>"#{home}/.local/share",
        'XDG_CACHE_HOME'=>"#{home}/.cache",'XDG_STATE_HOME'=>"#{home}/.local/state",'KDEHOME'=>"#{home}/.kde",
        'TMPDIR'=>runtime,'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=#{upstream}",
        'KWIN_MCP_REPO'=>"#{home}/no-repository",'CARGO'=>'/usr/bin/false','PATH'=>"#{wrappers}:#{ENV.fetch('PATH')}",
        'KW450_MODE'=>mode,'KW450_DAEMON'=>daemon}
      errors = File.open(File.join(evidence,"#{mode}.stderr"),'w')
      input,output,process = Open3.popen2(env,server,'--autoclean',err:errors)
      session = nil
      begin
        initialized = request(input,output,1,'initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'startup-diagnostics',version:'1'})
        raise initialized.inspect if initialized['error']
        input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized')); input.flush
        began = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        response = request(input,output,2,'tools/call',name:'session_start',arguments:{width:800,height:600,browser_profiles:'fresh'})
        elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC)-began
        File.write(File.join(evidence,"#{mode}.json"),JSON.pretty_generate(response))
        if mode == 'normal'
          started = data(response)
          session = started.fetch('session_id')
          workdir = started.fetch('workdir')
          raise 'real bus/Wayland sockets missing' unless File.socket?(File.join(workdir,'bus')) && File.socket?(File.join(workdir,'wayland-0'))
          data(request(input,output,3,'tools/call',name:'launch_app',arguments:{session_id:session,command:"kdialog --title kw450-normal --inputbox 'Startup diagnostic proof'"}))
          windows = data(request(input,output,4,'tools/call',name:'window_list',arguments:{session_id:session}))
          raise 'real dialog missing' unless JSON.generate(windows).include?('kw450-normal')
          capture = data(request(input,output,5,'tools/call',name:'screenshot',arguments:{session_id:session}))
          FileUtils.cp(capture.fetch('path'),File.join(evidence,'normal.png'))
          data(request(input,output,6,'tools/call',name:'session_stop',arguments:{session_id:session}))
          session = nil
          raise 'workdir retained' if File.exist?(workdir)
        else
          raise "startup did not fail: #{response}" unless response['error']
          unless baseline
            diagnostics = response.fetch('error').fetch('data')
            tail = diagnostics.fetch('sandbox_stderr_tail')
            raise 'real daemon failure missing' unless tail.include?('Failed to start message bus:') && tail.include?('broken')
            raise 'tail unbounded' unless tail.bytesize <= 4*16384
            raise 'wrong truncation flag' unless diagnostics['sandbox_stderr_truncated'] == (mode=='noisy')
            raise 'lossy UTF-8 tail missing' if mode=='noisy' && !tail.include?("kw450-tail\uFFFD")
            raise 'response omitted diagnostic text' unless response['error']['message'].include?('sandbox stderr')
            raise 'removed log was not preserved in response' if File.exist?(diagnostics.fetch('sandbox_stderr_path'))
            raise "early failure waited for the marker timeout: #{elapsed}" unless elapsed < 12
          end
        end
        listed = data(request(input,output,7,'tools/call',name:'session_list',arguments:{}))
        raise 'session handle retained' unless listed.fetch('sessions') == []
        Timeout.timeout(5) do
          sleep 0.02 until File.read("/proc/#{process.pid}/task/#{process.pid}/children").strip.empty?
        end
        raise 'owned runtime directory retained' unless Dir.glob(File.join(runtime,"kwin-mcp-#{process.pid}-*")).empty?
        raise 'owned disk directory retained' unless Dir.glob(File.join(home,'.cache/kwin-mcp',"kwin-mcp-#{process.pid}-*")).empty?
        puts "PASS #{mode}: elapsed=#{format('%.3f',elapsed)}s; real daemon, response and cleanup checked; baseline=#{baseline}"
      ensure
        request(input,output,8,'tools/call',name:'session_stop',arguments:{session_id:session}) if session
        input.close
        begin
          Timeout.timeout(20) { process.value }
        rescue Timeout::Error
          Process.kill('TERM',process.pid)
          Timeout.timeout(20) { process.value }
        ensure
          output.close
          errors.close
        end
      end
    end
  ensure
    Process.kill('TERM',upstream_pid) rescue Errno::ESRCH
    Process.wait(upstream_pid)
    upstream_log.close
  end
end
