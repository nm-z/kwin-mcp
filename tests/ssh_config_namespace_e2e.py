#!/usr/bin/env python3
"""Local stdio driver for disposable KWin native proofs; never uses host profiles."""
import json, os, queue, shlex, subprocess, sys, tempfile, threading, time
from pathlib import Path

class NativeKwin:
    def __init__(self, binary, evidence):
        self.evidence=Path(evidence); self.evidence.mkdir(parents=True,exist_ok=True)
        self.root=Path(tempfile.mkdtemp(prefix='kproof-',dir='/var/tmp'))
        self.home=self.root/'home'; self.home.mkdir(mode=0o700)
        for d in ['.config','.cache','.local/share','.local/state','.kde']:
            (self.home/d).mkdir(parents=True,exist_ok=True)
        runtime=os.environ.get('XDG_RUNTIME_DIR') or '/run/user/'+str(os.getuid())
        bus=os.environ.get('DBUS_SESSION_BUS_ADDRESS') or 'unix:path='+runtime+'/bus'
        self.proxy_log=open(self.evidence/'proxy.stderr','w')
        self.proxy=subprocess.Popen(['xdg-dbus-proxy',bus,str(self.root/'bus'),'--filter','--talk=org.freedesktop.systemd1','--talk=org.a11y.Bus'],stderr=self.proxy_log)
        deadline=time.monotonic()+5
        while not (self.root/'bus').exists():
            if self.proxy.poll() is not None or time.monotonic()>deadline: raise RuntimeError('filtered bus unavailable')
            time.sleep(.02)
        env=os.environ.copy()
        for key in ['DISPLAY','WAYLAND_DISPLAY','WAYLAND_SOCKET','XAUTHORITY']: env.pop(key,None)
        env.update(HOME=str(self.home),XDG_CONFIG_HOME=str(self.home/'.config'),XDG_CACHE_HOME=str(self.home/'.cache'),XDG_DATA_HOME=str(self.home/'.local/share'),XDG_STATE_HOME=str(self.home/'.local/state'),KDEHOME=str(self.home/'.kde'),XDG_RUNTIME_DIR=runtime,DBUS_SESSION_BUS_ADDRESS='unix:path='+str(self.root/'bus'),KWIN_MCP_REPO=str(self.root/'no-repository'),CARGO='/usr/bin/false')
        self.stderr=open(self.evidence/'server.stderr','w')
        self.process=subprocess.Popen([str(Path(binary).resolve()),'--autoclean','--no-viewer'],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.stderr,text=True,bufsize=1)
        self.q=queue.Queue(); self.serial=0; self.sessions={}; self.trace=open(self.evidence/'rpc.jsonl','w')
        def reader():
            for line in self.process.stdout:
                try:self.q.put(json.loads(line))
                except json.JSONDecodeError:self.q.put({'unparsed':line})
            self.q.put(None)
        threading.Thread(target=reader,daemon=True).start()
        self.rpc('initialize',{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'wingnut-native-proof','version':'1'}})
        self.process.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');self.process.stdin.flush()
    def rpc(self,method,params,timeout=180):
        self.serial+=1
        msg={'jsonrpc':'2.0','id':self.serial,'method':method,'params':params}
        self.trace.write(json.dumps({'request':msg})+'\n');self.trace.flush()
        self.process.stdin.write(json.dumps(msg)+'\n');self.process.stdin.flush()
        until=time.monotonic()+timeout
        while True:
            response=self.q.get(timeout=max(.01,until-time.monotonic()))
            if response is None:raise RuntimeError('MCP server exited')
            self.trace.write(json.dumps({'response':response})+'\n');self.trace.flush()
            if response.get('id')==self.serial:return response
    def call(self,name,args=None,session=None,allow_error=False):
        args=dict(args or {})
        if session is not None:args['session_id']=session
        response=self.rpc('tools/call',{'name':name,'arguments':args})
        if response.get('error') or response.get('result',{}).get('isError'):
            if allow_error:return response
            raise RuntimeError(json.dumps(response))
        result=response.get('result',{})
        return result.get('structuredContent',result)
    def start(self,**params):
        result=self.call('session_start',dict({'width':1280,'height':900,'browser_profiles':'fresh','startup_timeout_seconds':60},**params))
        sid=result['session_id'];self.sessions[sid]=result
        return result
    def stop(self,sid):
        result=self.call('session_stop',session=sid)
        self.sessions.pop(sid,None);return result
    def command(self,sid,command,label='command'):
        work=Path(self.sessions[sid]['workdir']); stem=work/(label+'-'+str(self.serial))
        stdout=Path(str(stem)+'.stdout');stderr=Path(str(stem)+'.stderr');status=Path(str(stem)+'.status')
        script='( '+command+' ) > '+shlex.quote(str(stdout))+' 2> '+shlex.quote(str(stderr))+'; result=$?; printf "%s\\n" "$result" > '+shlex.quote(str(status))
        launch=self.call('launch_app',{'command':'sh -c '+shlex.quote(script),'window_timeout_seconds':1},session=sid)
        until=time.monotonic()+20
        while not status.exists():
            if time.monotonic()>until:raise RuntimeError('command completion absent: '+json.dumps(launch))
            time.sleep(.05)
        result={'status':int(status.read_text().strip()),'stdout':stdout.read_text(),'stderr':stderr.read_text(),'launch':launch}
        (self.evidence/(label+'.json')).write_text(json.dumps(result,indent=2)+'\n')
        return result
    def close(self):
        for sid in list(self.sessions):
            try:self.stop(sid)
            except Exception as error:print('owned cleanup:',error,file=sys.stderr)
        try:self.process.stdin.close();self.process.wait(timeout=30)
        except Exception:
            self.process.terminate()
            try:self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:self.process.kill();self.process.wait()
        self.proxy.terminate()
        try:self.proxy.wait(timeout=5)
        except subprocess.TimeoutExpired:self.proxy.kill();self.proxy.wait()
        self.trace.close();self.stderr.close();self.proxy_log.close()


def system_snapshot():
    import hashlib
    paths=[Path('/etc/ssh/ssh_config'), *sorted(Path('/etc/ssh/ssh_config.d').glob('*.conf'))]
    result={}
    for path in paths:
        if not path.is_file(): continue
        stat=path.stat()
        result[str(path)]={'target':str(path.resolve()),'uid':stat.st_uid,'gid':stat.st_gid,'mode':stat.st_mode&0o7777,'device':stat.st_dev,'inode':stat.st_ino,'sha256':hashlib.sha256(path.read_bytes()).hexdigest()}
    return result

if __name__=='__main__':
    binary,evidence=sys.argv[1:3]
    before=system_snapshot()
    eligible=[p for p,s in before.items() if s['uid']==0 and not s['mode']&0o022]
    if not eligible: raise SystemExit('requires secure root-owned system SSH configuration')
    app=NativeKwin(binary,evidence)
    try:
        started=app.start()
        sid=started['session_id']
        parsed=app.command(sid,'ssh -G -F /etc/ssh/ssh_config localhost >/dev/null','system-config')
        assert parsed['status']==0,parsed
        assert 'Bad owner or permissions' not in parsed['stderr'],parsed
        ownership=app.command(sid,'stat -Lc "%u %a" '+ ' '.join(shlex.quote(p) for p in eligible),'config-ownership')
        assert ownership['status']==0,ownership
        assert all(line.split()==[str(os.getuid()),'600'] for line in ownership['stdout'].splitlines()),ownership
        work=Path(started['workdir'])
        (work/'check.conf').write_text('Include '+str(work/'insecure.conf')+'\n')
        (work/'insecure.conf').write_text('Host *\n    BatchMode yes\n')
        (work/'insecure.conf').chmod(0o666)
        refused=app.command(sid,'ssh -G -F '+shlex.quote(str(work/'check.conf'))+' localhost >/dev/null','insecure-include')
        assert refused['status']!=0 and 'Bad owner or permissions' in refused['stderr'],refused
        after=system_snapshot()
        assert before==after,'host system configuration changed'
        proof={'session':started,'host_before':before,'host_after':after,
               'secure_parse_exit':parsed['status'],'insecure_include_exit':refused['status'],
               'insecure_include_diagnostic':refused['stderr'],'host_unchanged':True}
        (Path(evidence)/'proof.json').write_text(json.dumps(proof,indent=2)+'\n')
        print('PASS: secure system SSH config parses; unsafe include is refused; host files unchanged')
    finally:
        app.close()
