import json, os, pathlib, socket, subprocess, sys, tempfile, time
binary=pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='zrun-valkey-test.') as work:
    root=pathlib.Path(work)
    sock=root/'valkey.sock'
    server=subprocess.Popen(['valkey-server','--port','0','--unixsocket',sock,'--unixsocketperm','700','--save','','--appendonly','no','--dir',root],stdout=subprocess.DEVNULL)
    try:
        for _ in range(200):
            if sock.is_socket(): break
            assert server.poll() is None
            time.sleep(.02)
        assert sock.is_socket()
        env={k:v for k,v in os.environ.items() if not k.startswith('ZRUN_')}
        env.update(ZRUN_SHELL='/bin/sh',ZRUN_VALKEY_SOCKET=str(sock),ZRUN_VALKEY_DOMAIN='team_a',ZRUN_VALKEY_TOOL_REVISION='tools-1', VALKEY_FIXTURE='env1')
        command={'script':'printf x >> count; printf "%s" "$VALKEY_FIXTURE"','reuse':{'scope':'fixture','input_revision':'v1','ttl_ms':3000}}
        def run(cmd=command, environ=env, count=1):
            input=(json.dumps(cmd)+'\n').encode()*count
            out=subprocess.run([binary,'batch'],input=input,cwd=root,env=environ,capture_output=True,check=True,timeout=7)
            return [json.loads(line) for line in out.stdout.splitlines()]
        def observe(expected, **kwargs):
            row=run(**kwargs)[0]
            assert row['disposition']==expected, row
            return row
        def connections():
            info = subprocess.check_output(['valkey-cli', '-s', sock, 'INFO', 'stats'], text=True)
            return int(next(line.split(':', 1)[1] for line in info.splitlines()
                            if line.startswith('total_connections_received:')))

        before = connections()
        observe('executed')
        assert connections() - before == 2  # one GET/SET connection and one INFO
        observe('cached')
        keys=subprocess.check_output(['valkey-cli','-s',sock,'--scan'],text=True).splitlines()
        assert len(keys)==1 and keys[0].startswith('zrun:v1:team_a:')
        ttl=int(subprocess.check_output(['valkey-cli','-s',sock,'PTTL',keys[0]]))
        assert 0<ttl<=3000
        observe('executed',environ=dict(env,ZRUN_VALKEY_DOMAIN='team_b'))
        observe('executed',environ=dict(env,ZRUN_VALKEY_TOOL_REVISION='tools-2'))
        assert observe('executed',environ=dict(env,VALKEY_FIXTURE='env2'))['result']['stdout']['data']=='env2'
        policy=root/'deny.json'
        policy.write_text('{"shell":"deny"}')
        denied=subprocess.run([binary,'batch'],input=(json.dumps(command)+'\n').encode(),cwd=root,env=dict(env,ZRUN_POLICY=str(policy)),capture_output=True,check=True,timeout=7)
        assert json.loads(denied.stdout)['disposition']=='rejected'
        no_cache=dict(command,reuse=dict(command['reuse'],ttl_ms=0))
        observe('executed',cmd=no_cache)
        observe('executed',cmd=no_cache)
        assert (root/'count').read_bytes()==b'x'*6
        time.sleep(ttl/1000+.05)
        observe('executed')
        server.terminate()
        server.wait(timeout=5)
        assert observe('executed')['result']['stdout']['data']=='env1'
        no_socket={k:v for k,v in env.items() if k!='ZRUN_VALKEY_SOCKET'}
        observe('executed',environ=no_socket)
        assert (root/'count').read_bytes()==b'x'*9
        unresponsive = root / 'unresponsive.sock'
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(unresponsive))
            listener.listen()
            script = root / 'outage.zrun'
            jobs = [
                dict(command, script='printf x',
                     reuse=dict(command['reuse'], input_revision=f'outage-{index}'))
                for index in range(16)
            ]
            script.write_text('# zrun:1\n' + ''.join(f'shell {json.dumps(job)}\n' for job in jobs))
            result = subprocess.run([binary, script], cwd=root,
                                    env=dict(env, ZRUN_VALKEY_SOCKET=str(unresponsive)),
                                    capture_output=True, check=True, timeout=10)
            assert result.stdout == b'x' * len(jobs)
            listener.settimeout(1)
            connection, _ = listener.accept()
            with connection:
                assert b'GET' in connection.recv(1024)
            listener.setblocking(False)
            try:
                unexpected, _ = listener.accept()
            except BlockingIOError:
                pass
            else:
                unexpected.close()
                raise AssertionError('broker retried Valkey inside the cooldown')
        print('Valkey restart hit, PX expiry, trust domain, tool and environment identities, zguard, TTL zero, outage cooldown and disabled mode passed')
    finally:
        if server.poll() is None:
            server.terminate(); server.wait(timeout=5)
