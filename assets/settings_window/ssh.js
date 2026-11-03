// Remote-work views share the existing window model and native command bridge.
let sshSelection = { peer:'', incoming:'' };
const sshText = (zh,en) => currentModel().language === 'en' ? en : zh;
const sshModel = () => currentModel().ssh || {data:{incoming:[],outgoing:[]},server:{accounts:[]},busy:false};
const sshStatus = value => ({pending:sshText('等待对方批准','Awaiting approval'),granting:sshText('正在授权／待核实','Authorizing / needs review'),authorized:sshText('已授权 · 待测试','Authorized · not tested'),ready:sshText('最近测试成功','Last test succeeded'),offline:sshText('设备暂不可达','Device unreachable'),preparing:sshText('正在准备 SSH 主机身份','Preparing SSH host identity'),recovery:sshText('对方有未完成授权','Unfinished approval on target'),conflict:sshText('对方已有旧授权','Existing access on target'),stale:sshText('对方已无此请求','Request no longer exists'),migration:sshText('需要重新授权','Reapproval required'),hostChanged:sshText('主机身份已变化','Host identity changed'),rejected:sshText('请求被拒绝','Request declined'),expired:sshText('请求已过期','Request expired'),cancelled:sshText('请求已取消','Request cancelled'),revoked:sshText('授权已撤销','Authorization revoked'),error:sshText('需要检查','Needs attention')})[value] || sshText('尚未授权','Not authorized');
const sshPriv = account => account ? (account.admin ? sshText('管理员','Administrator') : sshText('标准用户','Standard user')) : '—';
const sshTime = value => value ? new Date(value*1000).toLocaleString(currentModel().language === 'en' ? 'en-GB' : 'zh-CN') : sshText('尚未测试','Not tested');
const sshHasHan = value => /\p{Script=Han}/u.test(String(value||''));
const sshIssue = value => {
  const text=String(value||'');
  const translated=text.includes('OPENSSH_COMPONENT_MISMATCH') ? sshText('Windows 自带的 OpenSSH 无法正常工作。Pierdesk 可以改用自带的 SSH，不需要更新 Windows。','The Windows OpenSSH components cannot run correctly. Pierdesk can use its bundled SSH without updating Windows.') : ({'Effective SSH configuration is unavailable without administrator access.':sshText('需要完成管理员确认后才能读取有效的 SSH 配置。','Administrator confirmation is required to read the effective SSH configuration.'),'Multiple SSH ports require manual configuration.':sshText('检测到多个 SSH 端口，需要手动检查配置。','Multiple SSH ports require manual configuration.'),'SSH metadata needs to be refreshed by configuring access.':sshText('SSH 接入信息需要重新配置。','SSH access metadata needs to be configured again.')}[text] || text);
  return currentModel().language==='en'&&sshHasHan(translated)?'The local SSH configuration needs attention. Open the diagnostic log for the complete error.':translated;
};
const sshErrorText = value => {
  const text=String(value||'');
  if(text.includes('\uFFFD') && text.includes('Pierdesk SSH')) return sshText('旧版本未能生成 SSH 主机密钥，并且错误文字被错误解码。此版本已修正；请重新设置本机接入。','An older version failed to generate the SSH host key and decoded the error incorrectly. This version fixes both problems; set up local access again.');
  if(text.includes('OPENSSH_COMPONENT_MISMATCH')) return sshText('Windows 自带的 OpenSSH 组件不匹配。请重新设置本机接入，Pierdesk 会绕过这些组件并使用自带的 SSH；无需更新 Windows。','The Windows OpenSSH components do not match. Set up local access again and Pierdesk will bypass them with its bundled SSH; no Windows update is required.');
  if(text.includes('SSH state was restored from its backup')) return sshText('SSH 授权记录的主文件无法读取，Pierdesk 已从备份恢复。请核对授权列表；现有 SSH 密钥没有被修改。','The main SSH authorization file could not be read, so Pierdesk restored it from a backup. Review the authorization list; existing SSH keys were not changed.');
  if(text.includes('SSH authorization state is damaged')) return sshText('SSH 授权记录已损坏，且没有可用备份。为避免误删，Pierdesk 没有修改现有 SSH 密钥。请打开“管理接入”检查本机 SSH，再重新授权。','The SSH authorization record is damaged and no usable backup exists. To avoid deleting valid access, Pierdesk left existing SSH keys unchanged. Open Manage access to check local SSH before authorizing again.');
  if(text.includes('对方电脑有一项未完成的管理员授权') || text.includes('authorization recovery is required')) return sshText('对方电脑有一项未完成的管理员授权。请先在对方电脑完成或撤销该授权，再重试。','The target computer has an unfinished administrator approval. Complete or revoke it there, then retry.');
  if(text.includes('对方电脑已有这台设备的待处理或已授权记录') || text.includes('authorization already exists')) return sshText('对方电脑已有这台设备的旧记录。请先在对方电脑撤销旧记录，再重试。','The target computer already has an older request or authorization for this device. Revoke it there, then retry.');
  if(text.includes('对方电脑已找不到这项请求') || text.includes('Authorization request not found')) return sshText('对方电脑已找不到这项请求。可以直接重新申请，或在本机取消它。','The target computer no longer has this request. Submit it again, or cancel it locally.');
  if(text.includes('SSH 密钥发生变化') || text.includes('Authorization key changed')) return sshText('请求使用的 SSH 密钥已变化。请取消该请求后重新申请。','The SSH key for this request changed. Cancel the request, then submit a new one.');
  if(text.includes('SSH 协议版本不一致') || text.includes('SSH control protocol version')) return sshText('两台电脑的 Pierdesk 控制协议不一致。请让两台电脑使用同一版 Pierdesk 后重试。','The two computers use different Pierdesk control protocols. Run the same Pierdesk version on both computers, then retry.');
  if(text.includes('peer closed connection without sending TLS close_notify')) return sshText('对方电脑中断了这次 SSH 请求。请先确认对方 Pierdesk 正在运行，然后重试；旧请求会由程序自动清理。','The other computer interrupted this SSH request. Make sure Pierdesk is running there and retry; the app will clean up stale requests automatically.');
  if(text.includes('Account isolation check failed')) return sshText('旧版账户隔离检查误判了本机的其他账户。此版本已修正；请重新执行批准。','An older account-isolation check incorrectly rejected another local account. This version fixes that check; retry approval.');
  if(text.includes('cannot be ended safely from Pierdesk')) return sshText('Windows 没有提供可安全结束的单个 SSH 会话进程。请在对应终端输入 exit 结束连接，然后刷新状态。','Windows did not expose a single SSH session process that Pierdesk can end safely. Enter exit in the corresponding terminal, then refresh the status.');
  if(text.includes('Exception calling "Replace" with "3" argument')) return sshText('旧版本未能更新 SSH 授权文件。请使用最新版本重新执行批准或撤销。','An older version could not update the SSH authorization file. Retry approval or revocation with the latest version.');
  if(text.includes('Existing SSH configuration is invalid')) return sshText('旧版 SSH 配置未通过检查。此版本使用独立的 Pierdesk SSH；请重新设置本机接入。','The previous SSH configuration did not pass validation. This version uses independent Pierdesk SSH; set up local access again.');
  if(text.includes('SSH access needs reapproval after the Pierdesk dedicated SSH upgrade')) return sshText('此版本改用 Pierdesk 专用 SSH，需要重新申请一次。点击“重新申请”即可，本机连接工具会自动准备。','This version uses Pierdesk’s dedicated SSH service and requires one new approval. Select Request again; local connection tools are prepared automatically.');
  if(text.includes('dedicated Pierdesk SSH port')) return sshText('对方返回的不是 Pierdesk 专用 SSH 端口。请确认两台电脑都使用此版本后重新申请。','The target did not return Pierdesk’s dedicated SSH port. Run this version on both computers, then request access again.');
  if(text.includes('SSH host keys unavailable')) return sshText('对方正在准备 SSH 主机身份。Pierdesk 会自动重试，无需撤销或重新申请。','The target is preparing its SSH host identity. Pierdesk retries automatically; there is no need to revoke or submit again.');
  if(text.includes('valid independent SSH password')) return sshText('对方没有返回有效的独立 SSH 密码。请确认两台电脑使用同一版本，然后撤销并重新授权。','The target did not return a valid independent SSH password. Run the same version on both computers, then revoke and authorize again.');
  if(text.includes('private key cannot be unlocked')) return sshText('本机保存的 SSH 私钥无法用现有凭据解锁。请在目标电脑撤销该授权，然后重新申请。','The saved SSH private key cannot be unlocked with its credential. Revoke the authorization on the target, then request access again.');
  if(/Target SSH service is stopped/i.test(text)) return sshText('目标电脑的 SSH 服务已停止。请在该电脑打开“远程工作 → 本机接入”，修复接入后重试。','The target SSH service is stopped. On that computer, open Remote work > Local access, repair access, then retry.');
  if(/Target gateway firewall rule/i.test(text)) return sshText('目标电脑的认证入口规则缺失或与当前程序不匹配。请在该电脑的“远程工作 → 本机接入”中修复；无需修改网络类别。','The target gateway rule is missing or does not match the application. Repair Local access under Remote work on that computer; no network category change is needed.');
  if(/not authorized for this device and key/i.test(text)) return sshText('访问授权与当前设备或密钥不一致，或已被撤销。请在目标电脑核对这台设备的 SSH 授权；配对不会自动授予此权限。','SSH authorization does not match this device and key, or was revoked. Review this device’s SSH authorization on the target; pairing alone does not grant access.');
  if(/certificate|UnknownIssuer|BadSignature|fingerprint mismatch/i.test(text)) return sshText('设备身份校验失败，当前地址可能属于另一台电脑。请先核对设备配对信息，校验通过后才能连接。','Device identity verification failed; this address may belong to another computer. Review device pairing before connecting.');
  if(/SSH access.*(upgrade|attention)|Upgrade or repair local SSH/i.test(text)) return sshText('目标电脑的 SSH 接入需要升级或修复。请在该电脑打开“远程工作 → 本机接入”，完成 Windows 权限确认，再重新测试；已有账户授权会保留。','Upgrade or repair SSH access on the target under Remote work > Local access, confirm Windows permission there, then test again. Existing account authorizations are preserved.');
  if(/Open Pierdesk|Target offline/i.test(text)) return sshText('请在两台电脑运行 Pierdesk，再测试连接。','Run Pierdesk on both computers, then test the connection.');
  if(/version|Unsupported SSH control/i.test(text)) return sshText('两台电脑的 SSH 接入版本不兼容。请更新两端，并在目标电脑升级本机接入。','The SSH access versions are incompatible. Update both computers and upgrade local access on the target.');
  if(text.includes('无法连接对方电脑：') || text.includes('Device is offline or not discovered')) return sshText('无法连接对方电脑，原因未确认。请在两台电脑运行 Pierdesk；在目标电脑打开“远程工作 → 本机接入”，检查或修复接入。','Unable to reach the target; the cause is unconfirmed. Run Pierdesk on both computers. On the target, open Remote work > Local access to check or repair access.');
  if(text.includes('对方电脑拒绝了无效的 SSH 公钥') || text.includes('invalid SSH public key')) return sshText('对方电脑拒绝了无效的 SSH 公钥。请取消请求后重新申请。','The target computer rejected an invalid SSH public key. Cancel the request, then submit a new one.');
  if(text.includes('对方电脑无法处理这项 SSH 请求')) return sshText('对方电脑无法处理这项 SSH 请求，请在对方电脑查看远程工作状态。','The target computer could not process this SSH request. Check Remote work on that computer.');
  return currentModel().language==='en'&&sshHasHan(text)?'The SSH operation failed. Open the diagnostic log for the complete error.':text;
};
const sshActive = value => ['pending','granting','authorized','ready','offline','preparing','recovery','conflict','stale','migration','hostChanged','error'].includes(value);
const sshNeedsAction = value => ['recovery','conflict','stale','migration','error'].includes(value);
const sshOut = peer => [...sshModel().data.outgoing].reverse().find(c=>c.peer===peer && sshActive(c.status)) || [...sshModel().data.outgoing].reverse().find(c=>c.peer===peer);
const sshName = peer => (currentModel().peers || []).find(p=>p.fingerprint===peer)?.label || peer;
const sshVisibleAccount = account => /^[\p{L}\p{N}_.-]+$/u.test(account.name) && !['aspnet','defaultaccount','guest','wdagutilityaccount','defaultuser0','codexsandboxoffline','codexsandboxonline'].includes(account.name.toLowerCase());
const sshMutations = new Set(['request','approve','reject','cancel','revoke','setup','setup-from-review','setup-client','refresh','test','terminal','disconnect','trust']);
const sshButton = (action,label,id='',cls='',disabled=false) => `<button class="button ${cls}" data-ssh="${action}" data-id="${escapeHtml(id)}" ${disabled || (sshMutations.has(action) && (sshModel().busy || state.sshSubmitting)) ? 'disabled':''}>${label}</button>`;
const sshRequestButton = (peer,label,cls='primary',disabled=false) => `<button class="button ${cls}" data-ssh="request" data-peer="${escapeHtml(peer)}" ${disabled || sshModel().busy || state.sshSubmitting ? 'disabled':''}>${label}</button>`;
const sshCopyButton = (value,label,compact=false,disabled=false) => `<button class="ssh-inline-copy ${compact?'compact':''}" type="button" data-ssh-copy="${escapeHtml(value)}" aria-label="${escapeHtml(label)}" title="${escapeHtml(label)}" ${disabled?'disabled':''}>${svg('copy')}<span>${escapeHtml(compact?'':label)}</span></button>`;
const sshCopyableFact = (value,label) => `<span class="ssh-fact-value"><span>${escapeHtml(value)}</span>${sshCopyButton(value,label,true)}</span>`;
const sshHostKeyDisclosure = (id,title,keys,open=false) => `<details class="ssh-key-disclosure" id="${id}" ${open?'open':''}><summary><strong>${title}</strong><span class="ssh-key-visibility">${svg('eye','ssh-eye-show')}${svg('eye-off','ssh-eye-hide')}<b class="ssh-show-label">${sshText('显示','Show')}</b><b class="ssh-hide-label">${sshText('隐藏','Hide')}</b></span></summary><code>${escapeHtml(keys||'—')}</code></details>`;
const sshOperationText = operation => ({setup:sshText('正在配置 Pierdesk SSH，请在 Windows 权限窗口确认','Setting up Pierdesk SSH. Confirm the Windows permission prompt'),approve:sshText('正在写入这项授权，请在 Windows 权限窗口确认','Adding this authorization. Confirm the Windows permission prompt'),revoke:sshText('正在撤销授权，请在 Windows 权限窗口确认','Revoking access. Confirm the Windows permission prompt'),'setup-client':sshText('正在准备本机 SSH 连接工具','Preparing the SSH connection tools on this computer'),refresh:sshText('正在刷新本机 SSH 状态','Refreshing local SSH status'),test:sshText('正在验证 SSH 登录','Verifying the SSH login'),terminal:sshText('正在打开 SSH 终端','Opening the SSH terminal'),disconnect:sshText('正在断开 SSH 会话','Disconnecting the SSH session')})[operation] || sshText('正在完成操作','Completing the operation');
const sshLocalConfigured = server => Boolean(server?.installed && server?.running && server?.loopbackOnly && server?.firewall!=='missing' && (server.hostKeys||[]).length && !server.issue);
const sshCommandPreview = value => {const compact=String(value||'').replace(/\s+/g,' ').trim();return compact.length>112?`${compact.slice(0,112)}…`:compact;};
function sshAgentInstructions(c) {
  const target=sshName(c.peer), account=c.account?.name||'—', address=`${c.address}:${c.port}`, tested=sshTime(c.tested);
  if(currentModel().language === 'en') return `This is a Pierdesk remote-work handoff. Connect to the specified remote Windows computer first, then perform the user's requested work inside the same SSH session.

Connection information:
- Remote computer: ${target}
- Address: ${address}
- Account: ${account}
- Verified privileges: ${c.adminVerified?'Administrator':'Standard user'}
- Authentication: pinned SSH host identity and Pierdesk key with automatic local unlock
- Last successful test: ${tested}

What this access supports:
- Run CMD and PowerShell commands on the remote computer.
- Read, create, edit, move, and delete files allowed by the remote account.
- Use installed development tools, run scripts, build and test projects, and manage Git repositories.
- Start programs and background processes permitted by the remote SSH session.

Remote-work rules:
1. The current local project directory is unrelated to the remote filesystem. Unless the user explicitly asks for local work, perform all commands and file operations through this SSH session.
2. The remote shell starts as Windows CMD. A prompt such as "${account}@${target} C:\\Users\\${account}>" means the connection is active. Use CMD commands there, or run "powershell -NoProfile" before using PowerShell cmdlets.
3. Keep one interactive terminal session open until the task is complete. Do not reconnect for every command.
4. Pierdesk SSH access is separate from cross-screen control and clipboard permissions.

Connection procedure:
1. Run the complete command below exactly as provided in a local PowerShell terminal on this Windows computer. Use an interactive terminal when available.
2. Do not simplify it to "ssh -p ...". That shorter form omits Pierdesk's private key, pinned host identity, dedicated SSH client, and automatic unlock helper.
3. After signing in, run "hostname" and "whoami". Continue only when they match "${target}" and "${account}".
4. Change to the remote directory supplied by the user and perform the requested work. Clearly report remote paths, changes, and validation results.

Security and failure handling:
- The command works only on this computer and contains no private key or plaintext independent password. It does not use the Windows sign-in password.
- Do not print, export, delete, or rewrite Pierdesk configuration, keys, credentials, or authorization files.
- If connection or verification fails, stop and return the complete error unchanged.

Complete PowerShell command:
${c.command}`;
  return `这是 Pierdesk 生成的远程工作交接说明。请先连接指定的远程 Windows 电脑，再在同一个 SSH 会话中完成用户要求的工作。

连接信息：
- 远程电脑：${target}
- 地址：${address}
- 账户：${account}
- 已验证权限：${c.adminVerified?'管理员':'标准用户'}
- 认证方式：固定主机身份校验 + Pierdesk SSH 密钥，由本机自动解锁
- 最近测试成功：${tested}

这项远程访问可以用于：
- 在远程电脑执行 CMD 和 PowerShell 命令。
- 在远程账户权限范围内读取、创建、修改、移动和删除文件。
- 使用远程电脑已经安装的开发工具，运行脚本、编译、测试并管理 Git 仓库。
- 启动远程 SSH 会话允许的程序和后台进程。

远程工作边界：
1. 当前本机项目目录与远程文件系统没有关系。除非用户明确要求操作本机，否则所有命令和文件操作都必须通过这个 SSH 会话在远程电脑完成。
2. 登录后的默认终端是 Windows CMD。看到“${account}@${target} C:\\Users\\${account}>”一类提示符表示已经在远程电脑；此时应使用 CMD 命令。需要 PowerShell 命令时，先运行“powershell -NoProfile”。
3. 保持同一个交互终端会话直到任务完成，不要每执行一条命令就重新连接。
4. Pierdesk SSH 授权与跨屏控制、共享剪贴板权限相互独立。

连接步骤：
1. 在当前这台 Windows 电脑的本地 PowerShell 交互终端中原样运行下方完整命令。如果工具支持交互终端，应保持该终端会话。
2. 不要简化成“ssh -p ...”；普通命令缺少 Pierdesk 私钥、固定主机身份、专用 SSH 客户端和自动解锁程序。
3. 登录后先运行“hostname”和“whoami”。只有结果分别对应“${target}”和“${account}”时才继续。
4. 进入用户指定的远程目录完成任务，并清楚报告远程路径、修改内容和验证结果。

安全与失败处理：
- 完整命令只适用于当前电脑，不包含私钥或明文独立密码，也不使用 Windows 登录密码。
- 不要显示、导出、删除或改写 Pierdesk 的配置、密钥、凭据及授权文件。
- 如果连接或身份核对失败，立即停止，并原样返回完整错误。

完整 PowerShell 命令：
${c.command}`;
}
const sshSessionsFor = (id,direction) => (sshModel().sessions||[]).filter(session=>session.connectionId===id && session.direction===direction);
const sshElapsed = started => {
  const seconds=Math.max(0,Math.floor(Date.now()/1000)-Number(started||0));
  if(seconds<60)return sshText('不到 1 分钟','Less than a minute');
  if(seconds<3600)return sshText(`${Math.floor(seconds/60)} 分钟`,`${Math.floor(seconds/60)} min`);
  const hours=Math.floor(seconds/3600),minutes=Math.floor((seconds%3600)/60);
  return sshText(`${hours} 小时${minutes?` ${minutes} 分钟`:''}`,`${hours} hr${minutes?` ${minutes} min`:''}`);
};
function sshConnectionDiagram(remoteName,direction,sessions,address,account) {
  const connected=sessions.length>0, localName=currentModel().localLabel||sshText('本机','This PC');
  const route=direction==='incoming'?sshText('远程电脑 → 本机','Remote computer → this PC'):sshText('本机 → 远程电脑','This PC → remote computer');
  return `<section class="ssh-connection-overview ${connected?'connected':''}" aria-label="${sshText('SSH 连接状态','SSH connection status')}"><div class="ssh-route"><div class="ssh-route-device">${svg('monitor')}<small>${sshText('本机','This PC')}</small><strong>${escapeHtml(localName)}</strong></div><div class="ssh-route-line"><span>${svg('shield')}</span><b>${connected?sshText('SSH 已连接','SSH connected'):sshText('暂无活动连接','No active connection')}</b></div><div class="ssh-route-device">${svg('monitor')}<small>${sshText('远程电脑','Remote PC')}</small><strong>${escapeHtml(remoteName)}</strong></div></div><div class="ssh-route-meta"><span>${svg('clock')} ${connected?sshText(`已连接 ${sshElapsed(sessions[0].started)}`,`Connected ${sshElapsed(sessions[0].started)}`):sshText('已授权，可随时连接','Authorized and ready')}</span><span>${svg('link')} ${escapeHtml(address||'—')}</span><span>${svg('user')} ${escapeHtml(account||'—')}</span></div><p class="ssh-route-direction">${escapeHtml(route)}</p></section>`;
}
function sshSessionSection(sessions,options={}) {
  const rows=sessions.map((session,index)=>`<div class="ssh-session-row"><span class="ssh-session-dot"></span><div><strong>${sshText(`SSH 终端会话 ${index+1}`,`SSH terminal session ${index+1}`)}</strong><small>${sshText(`已连接 ${sshElapsed(session.started)}`,`Connected ${sshElapsed(session.started)}`)} · ${escapeHtml(session.address)}:${escapeHtml(session.port)}</small></div>${sshButton('disconnect',sessions.length===1?sshText('断开当前连接','Disconnect current session'):sshText('断开此连接','Disconnect session'),session.id,'danger')}</div>`).join('');
  const note=options.authorizationActive===false?sshText('访问权限已经撤销；这个已有会话仍需在这里单独断开。','Access has been revoked; this existing session must still be disconnected here.'):sshText('断开只结束所选终端会话，访问授权仍然保留。','Disconnecting ends only the selected terminal session; authorization remains in place.');
  return `<section class="ssh-session-section"><div class="ssh-section-heading"><h3>${sshText('活动会话','Active sessions')} <span>${sessions.length}</span></h3>${options.open?sshButton('terminal',sshText('打开新终端','Open new terminal'),options.id,'primary'):''}</div>${rows||`<div class="ssh-no-session"><span class="ssh-session-dot idle"></span><div><strong>${sshText('暂无活动连接','No active connection')}</strong><small>${sshText('授权仍然有效，需要时可以打开终端。','Authorization remains valid. Open a terminal when needed.')}</small></div></div>`}<p class="ssh-note">${note}</p></section>`;
}
function sshNotice() {
  const pending=sshModel().data.incoming.filter(g=>g.status==='pending');
  return pending.length ? `<div class="ssh-alert" role="status">${svg('shield')}<span>${sshText(`${pending.length} 台设备请求通过 SSH 访问本机`,`${pending.length} device(s) requesting SSH access`)}</span><button class="button ghost" data-ssh="review" data-id="${escapeHtml(pending[0].id)}">${sshText('查看请求','Review request')}</button></div>` : '';
}
function renderRemote() {
  const s=sshModel(), peers=currentModel().peers || [], incoming=s.data.incoming.filter(g=>sshActive(g.status)||sshSessionsFor(g.id,'incoming').length);
  const localConfigured=sshLocalConfigured(s.server);
  if(!sshSelection.peer && !sshSelection.incoming && peers.length) sshSelection.peer=peers[0].fingerprint;
  const grant=incoming.find(g=>g.id===sshSelection.incoming);
  const peer=peers.find(p=>p.fingerprint===sshSelection.peer);
  const connection=peer ? sshOut(peer.fingerprint) : null;
  const rows=peers.map(p=>{const c=sshOut(p.fingerprint),sessions=c?sshSessionsFor(c.id,'outgoing'):[];const detail=sessions.length?sshText(`SSH 已连接 · ${sessions.length} 个会话`,`SSH connected · ${sessions.length} session(s)`):c?.account?escapeHtml(c.account.name)+' · '+sshPriv(c.account):sshStatus(c?.status);return `<button class="ssh-device ${peer===p && !grant?'selected':''}" data-ssh="select-out" data-peer="${escapeHtml(p.fingerprint)}">${svg('monitor')}<span class="ssh-device-copy"><strong>${escapeHtml(p.label)}</strong><small><span class="dot ${sessions.length||p.online?'green':''}" style="display:inline-block"></span> ${p.online?sshText('在线','Online'):sshText('离线','Offline')} · ${detail}</small></span>${svg('chevron','ssh-chevron')}</button>`;}).join('');
  const inRows=incoming.map(g=>{const sessions=sshSessionsFor(g.id,'incoming');const status=sessions.length?sshText(`SSH 已连接 · ${sessions.length} 个会话`,`SSH connected · ${sessions.length} session(s)`):g.status==='authorized'?`${escapeHtml(g.account?.name || '')} · ${sshPriv(g.account)}`:sshStatus(g.status);return `<button class="ssh-device ${grant===g?'selected':''}" data-ssh="select-in" data-id="${escapeHtml(g.id)}">${svg('monitor')}<span class="ssh-device-copy"><strong>${escapeHtml(g.label)}</strong><small><span class="dot ${sessions.length?'green':''}" style="display:inline-block"></span> ${status}</small></span>${svg('chevron','ssh-chevron')}</button>`;}).join('');
  const error=s.error||s.listenerError||s.localProxyError||currentModel().sshError;
  const connectionError=String(connection?.error||''), pageError=String(error||'');
  const businessErrorCode={recovery:'authorization-recovery-required',conflict:'existing-authorization',stale:'request-not-found'}[connection?.status];
  const errorShownInConnection=sshNeedsAction(connection?.status) && connectionError && (pageError===connectionError || pageError.includes(connectionError) || connectionError.includes(pageError) || Boolean(businessErrorCode&&pageError.includes(businessErrorCode)));
  const setupError=error && ['setup','setup-client'].includes(s.operation);
  const localBusy=s.busy && ['setup','setup-client','approve','revoke','refresh'].includes(s.operation);
  return heading(sshText('远程工作','Remote work'),sshText('在已授权的电脑上，继续你的工作。','Continue your work on a computer you trust.'))+
    (error&&!errorShownInConnection&&state.modal?.type!=='ssh-local'?`<div class="ssh-error-card" role="alert"><div><strong>${sshText('上一步没有完成','The last operation did not finish')}</strong><span>${escapeHtml(sshErrorText(error))}</span></div>${setupError?`<button class="button ghost" data-ssh="local">${sshText('查看并重试','Review and retry')}</button>`:''}</div>`:'')+
    `<section class="ssh-workspace"><div class="ssh-list"><h2>${sshText('我可以访问的电脑','Computers I can access')}<span>${peers.length}</span></h2>${rows || `<p class="ssh-empty">${sshText('先在“设备与配对”中配对另一台电脑，再请求 SSH 访问。','Pair another computer in Devices & pairing, then request SSH access.')}</p><button class="button ghost" data-page="devices">${sshText('前往配对','Go to pairing')}</button>`}<hr class="ssh-divider"><h2>${sshText('允许访问本机的电脑','Computers accessing this device')}<span>${incoming.length}</span></h2>${inRows||`<p class="ssh-empty">${sshText('还没有设备获得 SSH 访问权限。配对不会自动授予此权限。','No device has SSH access. Pairing does not grant this permission.')}</p>`}</div><aside class="ssh-detail">${grant?sshIncomingDetail(grant):peer?sshOutgoingDetail(peer,connection):`<p class="ssh-empty">${sshText('选择一台电脑，查看访问权限和连接方式。','Select a computer to view access and connection options.')}</p>`}</aside></section>
    <article class="card ssh-local ${localConfigured?'configured':''}">${svg(localConfigured?'check':'shield')}<div class="ssh-local-copy"><h3>${sshText('本机接入','Local access')} · ${localConfigured?sshText('已配置，等待连接','Configured, awaiting connections'):s.server.installed?sshText('配置需要检查','Configuration needs attention'):sshText('尚未配置','Not configured')}</h3><span class="muted small">${localBusy?sshOperationText(s.operation):localConfigured?sshText(`SSH 服务正在 本机回环端口 ${s.server.port||45834} 运行 · 已授权 ${s.data.incoming.filter(x=>x.status==='authorized').length} 台设备`,`SSH listens on loopback port ${s.server.port||45834} · ${s.data.incoming.filter(x=>x.status==='authorized').length} approved device(s)`):sshText('配置后，获准的设备才能通过 SSH 连接本机','Approved devices can connect over SSH after local access is configured')}</span></div><button class="button ${localConfigured?'ghost':'primary'}" data-ssh="local">${localBusy?sshText('查看进度','View progress'):localConfigured?sshText('查看接入详情','View access details'):sshText('配置本机接入','Configure local access')}</button></article><p class="ssh-note">${sshText('两台电脑都需运行 Pierdesk。退出任一端会断开 SSH 会话；关闭设置窗口或最小化不影响连接。撤销授权阻止新连接，已有会话可单独断开。','Keep Pierdesk running on both computers. Quitting either end disconnects SSH sessions; closing settings or minimizing does not. Revocation blocks new connections; existing sessions can be disconnected separately.')}</p>`;
}
function sshTitle(name,subtitle) {return `<div class="ssh-detail-title">${svg('monitor')}<div><h2>${escapeHtml(name)}</h2><span class="muted small">${subtitle}</span></div></div>`;}
function sshOutgoingDetail(peer,c) {
  const sessions=c?sshSessionsFor(c.id,'outgoing'):[], hasSession=sessions.length>0;
  const usable=c && (['authorized','ready'].includes(c.status)||(c.status==='offline'&&c.account&&c.hostKeys?.length&&!c.offeredKeys?.length)), verified=c?.tested>0;
  const waiting=c && ['pending','granting','offline','preparing'].includes(c.status);
  let actions='';
  if((usable||hasSession) && !sshModel().server.clientInstalled) actions=sshButton('setup-client',sshText('修复本机连接工具','Repair connection tools'),'','primary');
  else if(usable||hasSession) {
    const account=`${c.account?.name||'—'} · ${sshPriv(c.account)}`;
    const address=c.address?`${c.address}:${c.port}`:'—';
    const authorized=usable&&!['revoked','cancelled','expired','rejected'].includes(c.status);
    actions=`${sshConnectionDiagram(peer.label,'outgoing',sessions,address,account)}${sshSessionSection(sessions,{open:authorized,id:c.id,authorizationActive:authorized})}<section class="ssh-access-section"><div><span>${sshText('访问授权','Access authorization')}</span><strong>${authorized?sshText('已授权','Authorized'):sshText('已撤销或失效','Revoked or unavailable')}</strong><small>${authorized?sshText('允许本机以后继续通过 SSH 登录这台电脑。','This computer may continue to sign in to the remote computer over SSH.'):sshText('不能再打开新连接；已有会话仍需单独断开。','New connections are blocked; existing sessions must be disconnected separately.')}</small></div></section>${authorized?`<section class="ssh-tool-section"><div><span>${sshText('连接与交接','Connect and hand off')}</span><strong>${sshText('使用这项授权','Use this authorization')}</strong><small>${sshText('查看连接参数、交给另一个 agent，或重新验证实际连接。','View connection details, hand off to another agent, or verify the live connection again.')}</small></div><div class="ssh-tool-actions">${sshButton('details',sshText('连接详情','Connection details'),c.id,'ghost')}${sshButton('agent',sshText('复制给 agent','Copy for agent'),c.id,'ghost',!verified)}${sshButton('test',sshText('测试连接','Test connection'),c.id,'ghost')}${sshButton('network',sshText('检查连接条件','Check connection requirements'),c.id,'ghost')}</div></section>`:''}`;
  }
  else if(waiting) actions=`${sshButton('waiting',sshText('查看请求进度','View request'),c.id,'primary')}${c.account?'':sshButton('cancel',sshText('取消请求','Cancel request'),c.id,'ghost')}`;
  else if(sshNeedsAction(c?.status)) actions=`${sshRequestButton(peer.fingerprint,['stale','migration'].includes(c.status)?sshText('重新申请','Submit again'):sshText('处理后重试','Retry after resolving'))}${sshButton('cancel',sshText('在本机取消','Cancel locally'),c.id,'ghost')}<p class="ssh-note">${sshText('已停止自动重试，避免反复出错。按上方提示处理后再重试；即使对方离线，也可以先在本机取消这项请求。','Automatic retries have stopped to prevent repeated failures. Resolve the issue above and retry; you can always cancel this request locally, even while the target is offline.')}</p>`;
  else if(c?.status==='hostChanged') actions=sshButton('host-review',sshText('核对新的主机身份','Review new host identity'),c.id,'primary');
  else actions=`${sshRequestButton(peer.fingerprint,c?sshText('重新申请','Request again'):sshText('请求访问','Request access'),'primary',!peer.online)}<p class="ssh-note">${sshText('点击后会在本机静默准备连接工具。请在对方电脑打开“远程工作”，核对当前 Windows 账户并批准；首次接入需要在对方电脑确认 Windows 权限提示。','Connection tools are prepared silently on this computer. On the other computer, open Remote work, verify the current Windows account and approve; initial setup requires a Windows permission confirmation there.')}</p>`;
  const summary=(usable||hasSession)?'':`<dl class="ssh-facts"><dt>${sshText('登录账户','Login account')}</dt><dd>${escapeHtml(c?.account?.name || '—')}</dd><dt>${sshText('远端权限','Remote privileges')}</dt><dd>${sshPriv(c?.account)}${c?.account&&!verified?` <span class="small muted">${sshText('（待验证）','(unverified)')}</span>`:''}</dd><dt>${sshText('认证方式','Authentication')}</dt><dd>${sshText('SSH 密钥 + 独立密码','SSH key + independent password')}</dd><dt>${sshText('最近测试','Last tested')}</dt><dd>${escapeHtml(sshTime(c?.tested))}${verified?' · '+sshText('成功','success'):''}</dd></dl>`;
  return sshTitle(peer.label,escapeHtml(hasSession?sshText('SSH 已连接','SSH connected'):sshStatus(c?.status)))+summary+`${c?.error?`<p class="ssh-error">${escapeHtml(sshErrorText(c.error))}</p>`:''}<div class="ssh-actions ${usable||hasSession?'ssh-dashboard-actions':''}">${actions}</div>`;
}
function sshIncomingDetail(g) {
  const sessions=sshSessionsFor(g.id,'incoming'), authorized=g.status==='authorized';
  if(authorized||sessions.length) {
    const account=`${g.account?.name||'—'} · ${sshPriv(g.account)}`;
    const address=sessions.length?sshText(`来自 ${sessions[0].address} · 本机:${sessions[0].port}`,`From ${sessions[0].address} · this PC:${sessions[0].port}`):sshText(`本机:${sshModel().server.port||45834}`,`This PC:${sshModel().server.port||45834}`);
    const revoke=authorized?sshButton('revoke-review',sshText('撤销访问权限','Revoke access'),g.id,'danger'):'';
    return sshTitle(g.label,escapeHtml(sessions.length?sshText('SSH 已连接','SSH connected'):sshText('已允许访问本机','Access to this computer allowed')))+`<div class="ssh-dashboard-actions">${sshConnectionDiagram(g.label,'incoming',sessions,address,account)}${sshSessionSection(sessions,{authorizationActive:authorized})}<section class="ssh-access-section"><div><span>${sshText('访问授权','Access authorization')}</span><strong>${authorized?sshText('已授权','Authorized'):sshText('已撤销','Revoked')}</strong><small>${authorized?sshText('允许这台设备以后继续通过 SSH 登录本机。','This device may continue to sign in to this computer over SSH.'):sshText('以后登录已被阻止；当前会话需单独断开。','Future sign-ins are blocked; current sessions must be disconnected separately.')}</small></div><div class="ssh-access-actions">${sshButton('grant-details',sshText('授权详情','Authorization details'),g.id,'ghost')}${revoke}</div></section></div>`;
  }
  const actions=g.status==='pending'
    ? `<button class="button primary" data-ssh="review" data-id="${escapeHtml(g.id)}">${sshText('审核访问请求','Review access request')}</button>`
    : g.status==='granting'
      ? `<button class="button primary" data-ssh="review" data-id="${escapeHtml(g.id)}">${sshText('重新完成授权','Retry authorization')}</button>${sshButton('revoke-review',sshText('撤销这项请求','Revoke request'),g.id,'danger')}`
      : sshButton('revoke-review',g.status==='migration'?sshText('清除旧记录','Clear old record'):sshText('撤销访问权限','Revoke access'),g.id,'danger');
  return sshTitle(g.label,escapeHtml(g.status==='authorized'?sshText('已允许访问本机','Access to this computer allowed'):sshStatus(g.status)))+`<dl class="ssh-facts"><dt>${sshText('本机账户','Local account')}</dt><dd>${escapeHtml(g.account?.name||'—')}</dd><dt>${sshText('账户权限','Account privileges')}</dt><dd>${sshPriv(g.account)}</dd><dt>${sshText('授权方向','Access direction')}</dt><dd>${sshText('这台设备 → 本机','This device → this computer')}</dd></dl><div class="ssh-actions">${actions}<p class="ssh-note">${sshText('SSH 授权与跨屏控制、剪贴板权限互相独立。','SSH access is independent of cross-screen and clipboard permissions.')}</p></div>`;
}
function renderSshModal(modal) {
  const s=sshModel(), g=s.data.incoming.find(x=>x.id===modal.id), c=s.data.outgoing.find(x=>x.id===modal.id);
  let title='',body='',buttons='';
  if(modal.type==='ssh-local') {
    title=s.server.installed&&!s.server.loopbackOnly?sshText('升级 SSH 接入','Upgrade SSH access'):sshText('本机 SSH 接入','SSH access to this computer');
    const configured=sshLocalConfigured(s.server);
    if(s.busy && ['setup','setup-client','refresh'].includes(s.operation)) {const progressDetail=s.operation==='refresh'?sshText('弹窗会保持打开，并在读取完成后更新下面的信息。','This dialog stays open and updates the information below when the refresh finishes.'):s.operation==='setup-client'?sshText('连接工具会保存在当前用户目录，无需管理员确认。完成后页面会自动更新。','The connection tools are stored for the current user and need no administrator confirmation. The page updates when finished.'):sshText('Pierdesk 正在准备自带的 SSH 文件并配置独立服务。你可以关闭此窗口，完成后页面会自动更新。','Pierdesk is preparing its bundled SSH files and configuring an independent service. You may close this dialog; the page updates when finished.');body=`<div class="ssh-progress" role="status">${svg('shield')}<div><strong>${sshOperationText(s.operation)}</strong><span>${progressDetail}</span></div></div>`;}
    body+=`${configured?`<div class="ssh-setup-state" role="status">${svg('check')}<div><strong>${sshText('已配置，等待连接','Configured, awaiting connections')}</strong><span>${sshText(`Pierdesk SSH 正在专用端口 ${s.server.port||45834} 运行。认证入口已准备；真实可达性以连接测试为准。`,`Pierdesk SSH is running on dedicated port ${s.server.port||45834}. The gateway is prepared; a connection test verifies reachability.`)}</span></div></div>`:`<div class="ssh-setup-state pending" role="status">${svg('shield')}<div><strong>${s.server.installed?sshText('本机接入配置需要检查','Local access configuration needs attention'):sshText('尚未配置本机接入','Local access is not configured')}</strong><span>${sshText('配置后，两台电脑运行 Pierdesk 时即可使用获准的 SSH 连接。','After setup, approved SSH connections are available while Pierdesk runs on both computers.')}</span></div></div>`}<p>${sshText('普通身份打开 Pierdesk 即可。设置服务和批准账户时，Windows 会要求管理员确认；Pierdesk 不保存管理员密码。','Run Pierdesk normally. Windows requests administrator confirmation when setting up the service or approving an account; Pierdesk does not store administrator passwords.')}</p><dl class="ssh-facts"><dt>SSH</dt><dd>${s.server.running?sshText('运行中','Running'):sshText('未运行','Stopped')}</dd><dt>${sshText('专用端口','Dedicated port')}</dt><dd>${s.server.port||45834}</dd><dt>${sshText('已授权设备','Approved devices')}</dt><dd>${s.data.incoming.filter(x=>x.status==='authorized').length}</dd></dl><p class="ssh-note">${sshText('设置后 SSH 仅在本机监听。公用和专用网络允许本地子网设备接触 Pierdesk 认证入口，但配对不会自动授予 SSH 权限。只有获准设备才能进入 SSH；账户授权会跟随设备，换网无需重新批准。首次设置或升级需在本机确认 Windows 权限提示。升级会重启 SSH 服务并中断已有连接。','SSH listens only on this computer. On public and private networks, the local subnet can reach Pierdesk’s authenticated gateway. Pairing alone does not grant SSH access. Approved device identities keep their authorization when networks change. Setup or upgrade requests local Windows permission; upgrading restarts SSH and interrupts existing connections.')}</p><p class="ssh-note">${sshText('第一次 Windows 权限提示需要在这台电脑上操作；当前跨屏控制不能操作 UAC 安全桌面。','The first Windows permission prompt must be handled on this computer. Current cross-screen control cannot operate the UAC secure desktop.')}</p>${s.error&&['setup','setup-client','refresh'].includes(s.operation)?`<div class="ssh-inline-error"><strong>${s.operation==='refresh'?sshText('刷新没有完成','Refresh did not finish'):sshText('设置没有完成','Setup did not finish')}</strong><span>${escapeHtml(sshErrorText(s.error))}</span></div>`:''}${s.server.issue&&!(s.error&&['setup','setup-client','refresh'].includes(s.operation))?`<p class="ssh-error">${escapeHtml(sshIssue(s.server.issue))}</p>`:''}`;
    const firewall={configured:sshText('Pierdesk 规则已启用；连接测试才代表实际可达。','Pierdesk rules are enabled; a connection test confirms reachability.'),missing:sshText('本机接入规则未配置。','Local access rules are not configured.'),unknown:sshText('尚未确认防火墙状态，可通过设置接入后测试连接。','Firewall state is unconfirmed. Configure access, then test the connection.')}[s.server.firewall||'unknown'];
    body+=`<p class="ssh-note">${firewall}</p>${sshHostKeyDisclosure('ssh-host-keys',sshText('主机公钥（供另一台电脑核对）','Host public keys (verify on the other computer)'),(s.server.hostKeys||[]).join('\n'),state.sshHostKeysOpen)}`;
    buttons=sshButton('refresh',sshText('刷新状态','Refresh status'),'','ghost')+sshButton('setup',s.server.installed&&!s.server.loopbackOnly?sshText('升级 SSH 接入','Upgrade SSH access'):s.server.installed?sshText('修复本机接入','Repair local access'):sshText('配置本机接入','Configure local access'),'','primary')+sshButton('diagnostics',sshText('复制诊断信息','Copy diagnostics'),'','ghost');
  } else if(modal.type==='ssh-network') {
    title=sshText('检查连接条件','Check connection requirements');
    const path=s.remoteAccess?.[modal.id], network=path?.network;
    body=`<p>${sshText('请在目标电脑打开 Pierdesk，进入“远程工作 → 本机接入”。若提示升级或修复，请在目标电脑完成 Windows 权限确认。完成后，在这里点击“测试连接”。','On the target, open Pierdesk > Remote work > Local access. Upgrade or repair access and confirm Windows permission there, then test the connection here.')}</p><p>${sshText('若仍无法连接，原因可能是网络隔离或管理员策略；当前不能仅凭超时确定防火墙故障。无需把网络改为专用。','If access still fails, network isolation or administrator policy may be involved. A timeout alone does not prove a firewall fault. There is no need to change the network to private.')}</p>`;
    if(path)body+=`<p class="ssh-note">${sshText('最近验证的目标路径','Last verified target path')}: ${escapeHtml(path.localAddress||'—')} · ${escapeHtml(network?.name||'—')} · ${escapeHtml(network?.category||'—')}</p>`;
    buttons=sshButton('refresh',sshText('重新检查本机','Recheck this computer'),'','ghost')+sshButton('local',sshText('本机接入','Local access'),'','ghost')+sshButton('diagnostics',sshText('复制诊断信息','Copy diagnostics'),'','primary');
  } else if(modal.type==='ssh-review' && g) {
    title=sshText('允许访问这台电脑？','Allow access to this computer?');
    const retrying=g.status==='granting';
    const accounts=(s.server.accounts || []).filter(sshVisibleAccount), account=accounts[0];
    modal.sid=account?.sid||'';
    body=`<p><strong>${escapeHtml(g.label)}</strong> ${sshText('请求通过 SSH 使用当前 Windows 账户。','is requesting SSH access to the current Windows account.')}</p>${account?`<div class="ssh-current-account"><span>${sshText('当前 Windows 账户','Current Windows account')}</span><strong>${escapeHtml(account.name)} · ${sshPriv(account)}</strong></div>`:`<div class="ssh-inline-error"><strong>${sshText('当前账户暂不支持 SSH 授权','The current account cannot be approved')}</strong><span>${sshText('请使用已启用的本地 Windows 账户打开 Pierdesk。','Open Pierdesk from an enabled local Windows account.')}</span></div>`}${retrying?`<div class="ssh-prerequisite"><strong>${sshText('上一次管理员操作没有完成','The previous administrator operation did not finish')}</strong><span>${sshText('可以安全重试；Pierdesk 会复用原账户和密钥，不会重复授权。','You can safely retry. Pierdesk reuses the original account and key without creating a duplicate authorization.')}</span></div>`:''}<div class="ssh-alert ssh-permission-note">${svg('shield')}<span>${account?.admin?sshText('此账户拥有管理员权限，可安装软件、修改系统和访问账户允许的文件。','This account has administrator privileges and can install software, change the system and access permitted files.'):sshText('对方将使用此账户已有的权限。授权不会把标准账户变成管理员。','The other device receives this account’s existing privileges. Approval does not turn a standard account into an administrator.')}</span></div><p class="ssh-note">${sshText('SSH 必须登录一个具体账户，因此 Pierdesk 只授权当前 Windows 账户。如需使用其他账户，请先切换 Windows 登录账户，再打开 Pierdesk。','SSH must sign in to a specific account, so Pierdesk approves only the current Windows account. To use another account, sign in to Windows as that account and open Pierdesk.')}</p><p class="ssh-note">${sshText('Pierdesk 会为这项授权生成独立密码并与 SSH 密钥配合使用；不会读取、发送或修改 Windows 登录密码。正式授权时，Windows 会在本机请求管理员确认。','Pierdesk generates an independent password for this authorization and uses it with the SSH key. It never reads, sends or changes the Windows sign-in password. Windows requests administrator confirmation on this computer when access is approved.')}</p>${!sshLocalConfigured(s.server)?`<div class="ssh-prerequisite"><strong>${sshText('还差一步：先设置本机 SSH 接入','One step first: set up local SSH access')}</strong><span>${sshText('完成本机接入设置后会自动回到这里，再批准当前账户。','After local access setup completes, this request returns here so you can approve the current account.')}</span></div>`:''}`;
    body+=`<p class="ssh-note">${sshText('当前账户名可以使用中文、字母、数字、点、横线或下划线。','The current account name may use Unicode letters, numbers, dots, hyphens or underscores.')}</p>`;
    buttons=sshButton('reject',sshText('拒绝','Decline'),g.id,'ghost',g.status!=='pending')+(sshLocalConfigured(s.server)?sshButton('approve',svg('shield')+(retrying?sshText('重新执行管理员授权','Retry administrator authorization'):sshText('管理员批准并授权','Approve with administrator permission')),g.id,'primary',!account||!['pending','granting'].includes(g.status)):sshButton('setup-from-review',sshText('设置本机接入并继续','Set up local access and continue'),g.id,'primary'));
  } else if(modal.type==='ssh-waiting') {
    const actionable=sshNeedsAction(c?.status);
    title=actionable?sshText('需要处理后继续','Action required'):sshText('等待对方批准','Waiting for approval');
    if(actionable) {
      const guidance={recovery:sshText('请在目标电脑打开“远程工作”，完成上次管理员授权，或撤销那项记录。','On the target computer, open Remote work and finish the previous administrator approval, or revoke that record.'),conflict:sshText('请在目标电脑的“允许访问本机的电脑”中撤销这台设备的旧记录。','On the target computer, revoke the old entry for this device under Computers accessing this device.'),stale:sshText('对方已清理这项请求，可以直接重新申请。','The target has cleared this request. You can submit it again now.'),migration:sshText('此版本改用独立的 Pierdesk SSH。点击重新申请，对方批准一次即可。','This version uses the independent Pierdesk SSH service. Submit again and approve it once on the target.'),error:sshText('请按错误提示检查两台电脑，然后重试。','Check both computers using the error below, then retry.')}[c?.status];
      body=`<p>${escapeHtml(guidance||'')}</p><div class="ssh-wait-state" role="alert">${svg('info')}<div><strong>${escapeHtml(sshStatus(c?.status))}</strong><span>${escapeHtml(sshErrorText(c?.error)||guidance||'')}</span></div></div><p class="ssh-note">${sshText('已停止自动检查，不会再每 5 秒重复出错。处理完成后点击重试；也可以随时在本机取消。','Automatic checks have stopped, so this error will not repeat every five seconds. Resolve it and retry, or cancel locally at any time.')}</p>`;
      buttons=sshRequestButton(c?.peer||'',c&&['stale','migration'].includes(c.status)?sshText('重新申请','Submit again'):sshText('已处理，重试','Resolved, retry'))+sshButton('cancel',sshText('在本机取消','Cancel locally'),c?.id||'','ghost');
    } else {
      const approvalStarted=Boolean(c?.account);
      body=`<p>${approvalStarted?sshText('目标电脑已经开始授权。Pierdesk 会自动检查结果；如需停止，请在目标电脑撤销这项授权。','The target computer has started authorization. Pierdesk checks the result automatically; revoke it on the target computer if you need to stop.'):sshText('请求已经送到目标电脑。请在目标电脑的“远程工作”页面核对当前 Windows 账户、设置本机 SSH，并完成 Windows 权限确认。','The request reached the target computer. On that computer, verify the current Windows account, set up local SSH and complete the Windows permission prompt.')}</p><div class="ssh-wait-state" role="status"><span class="ssh-wait-dot"></span><div><strong>${escapeHtml(sshStatus(c?.status))}</strong><span>${sshText('Pierdesk 每 5 秒自动检查一次，无需反复点击。','Pierdesk checks automatically every 5 seconds; no repeated clicks are needed.')}</span></div></div><p class="ssh-note">${approvalStarted?sshText('关闭此窗口不会中断授权。','Closing this dialog does not interrupt authorization.'):sshText('请求有效期为 10 分钟。关闭此窗口不会取消请求。','Requests expire after 10 minutes. Closing this dialog does not cancel the request.')}</p>`;
      buttons=approvalStarted?'':sshButton('cancel',sshText('取消请求','Cancel request'),c?.id||'','ghost');
    }
  } else if(modal.type==='ssh-grant-details' && g) {
    title=sshText('授权详情','Authorization details');
    body=`<dl class="ssh-facts ssh-connection-facts"><dt>${sshText('设备','Device')}</dt><dd>${escapeHtml(g.label)}</dd><dt>${sshText('本机账户','Local account')}</dt><dd>${sshCopyableFact(g.account?.name||'—',sshText('复制账户','Copy account'))}</dd><dt>${sshText('账户权限','Account privileges')}</dt><dd>${sshPriv(g.account)}</dd><dt>${sshText('授权方向','Access direction')}</dt><dd>${sshText('这台设备 → 本机','This device → this computer')}</dd><dt>${sshText('认证方式','Authentication')}</dt><dd>${sshText('SSH 密钥 + 独立密码','SSH key + independent password')}</dd><dt>${sshText('登录密码','Password')}</dt><dd>${sshText('由本机生成，仅安全发送给请求设备','Generated here and securely sent only to the requesting device')}</dd><dt>${sshText('授权状态','Authorization status')}</dt><dd>${g.status==='authorized'?sshText('已授权','Authorized'):sshText('已撤销','Revoked')}</dd></dl><h3>${sshText('设备登录公钥','Device login public key')}</h3><div class="ssh-code-block">${sshCopyButton(g.publicKey,sshText('复制公钥','Copy public key'),false,!g.publicKey)}<code>${escapeHtml(g.publicKey||'—')}</code></div><p class="ssh-note">${sshText('这里只显示可公开核对的公钥。独立密码不会使用或修改 Windows 登录密码；私钥始终保存在请求访问的电脑上。','Only the public key used for verification is shown. The independent password never uses or changes the Windows sign-in password; the private key remains on the requesting computer.')}</p>`;
  } else if(modal.type==='ssh-revoke' && g) {
    title=g.status==='migration'?sshText('清除这项旧 SSH 记录？','Clear this old SSH record?'):sshText('撤销这台设备的 SSH 权限？','Revoke SSH access for this device?');
    body=g.status==='migration'?`<p>${escapeHtml(g.label)}</p><p>${sshText('这项记录来自旧版 Windows SSH 流程，尚未写入 Pierdesk 专用服务。清除记录无需管理员确认；如需访问，可以由对方重新申请。','This record came from the retired Windows SSH flow and was not added to Pierdesk’s dedicated service. Clearing it needs no administrator confirmation; the other computer can submit a new request if needed.')}</p>`:`<p>${escapeHtml(g.label)} · ${escapeHtml(g.account?.name||'')}</p><p>${sshText('确认 Windows 权限提示后，会删除这台设备的登录密钥。其他设备的授权会保留。','After Windows approval, this device’s login key will be removed. Other devices keep their access.')}</p><p class="ssh-note">${sshText('撤销阻止新的登录，已经打开的 SSH 会话不会自动结束。','Revocation prevents new logins. Existing SSH sessions do not end automatically.')}</p>`;
    buttons=sshButton('revoke',g.status==='migration'?sshText('清除记录','Clear record'):sshText('撤销访问权限','Revoke access'),g.id,'danger');
  } else if(modal.type==='ssh-host' && c) {
    modal.expected=c.offeredKeys.join('\n');
    title=sshText('主机身份发生变化','Host identity changed');
    body=`<p>${sshText('请先在目标电脑核对 SSH 主机公钥。可能是重装了 SSH，也可能连接到了其他电脑。核对前暂停连接。','Verify the SSH host public keys on the target computer. SSH may have been reinstalled, or this may be a different computer. Connections are paused until reviewed.')}</p><h3>${sshText('原公钥','Previous public keys')}</h3><code>${escapeHtml((c.hostFingerprints||[]).join('\n')+'\n'+c.hostKeys.join('\n'))}</code><h3>${sshText('新公钥','New public keys')}</h3><code>${escapeHtml((c.offeredFingerprints||[]).join('\n')+'\n'+c.offeredKeys.join('\n'))}</code>`;
    buttons=sshButton('trust',sshText('已核对，接受新身份','Verified · accept new identity'),c.id,'primary');
  } else if(c) {
    title=sshText('连接详情','Connection details');
    const account=c.account?.name||'—';
    const loginPassword=s.loginPasswords?.[c.id]||'';
    const passwordField=loginPassword?`<span class="ssh-secret-value"><span class="ssh-secret-mask" aria-label="${sshText('登录密码已隐藏','Login password hidden')}">••••••••••••••••</span>${sshCopyButton(loginPassword,sshText('复制登录密码','Copy login password'),true)}</span>`:'—';
    body=`<dl class="ssh-facts ssh-connection-facts"><dt>${sshText('目标地址','Address')}</dt><dd>${sshCopyableFact(c.address,sshText('复制目标地址','Copy address'))}</dd><dt>${sshText('端口','Port')}</dt><dd>${sshCopyableFact(String(c.port),sshText('复制端口','Copy port'))}</dd><dt>${sshText('账户','Account')}</dt><dd>${sshCopyableFact(account,sshText('复制账户','Copy account'))}</dd><dt>${sshText('权限','Privileges')}</dt><dd>${sshPriv(c.account)}</dd><dt>${sshText('认证方式','Authentication')}</dt><dd>${sshText('SSH 密钥 + 独立密码','SSH key + independent password')}</dd><dt>${sshText('登录密码','Password')}</dt><dd>${passwordField}</dd><dt>${sshText('私钥','Private key')}</dt><dd>${sshText('加密保存在本机，不显示或导出','Encrypted locally; never displayed or exported')}</dd></dl><div class="ssh-credential-note">${svg('shield')}<span>${sshText('独立密码由目标电脑生成，只在本机解锁这项授权的私钥；它不是 Windows 登录密码。','The independent password is generated by the target and only unlocks this authorization key locally. It is not the Windows sign-in password.')}</span></div><section class="ssh-compact-help"><strong>${sshText('怎样连接','How to connect')}</strong><span>${sshText('在“远程工作”页面点击“打开新终端”，或复制下方完整命令并粘贴到本机 PowerShell。连接会自动校验目标并解锁密钥。','Select Open new terminal on the Remote work page, or copy the complete command below into PowerShell on this computer. The connection verifies the target and unlocks the key automatically.')}</span></section><p class="ssh-command-label">${sshText('本机专用的完整 PowerShell 命令','Complete PowerShell command for this computer')}</p><div class="ssh-code-block ssh-command-block"><code aria-label="${sshText('完整命令已折叠显示','Complete command shown in collapsed form')}">${escapeHtml(sshCommandPreview(c.command))}</code></div><p class="ssh-note ssh-command-note">${sshText('这里只显示一行预览；点击底部“复制 SSH 命令”会复制完整内容。','Only a one-line preview is shown here. Copy SSH command below copies the complete content.')}</p>${sshHostKeyDisclosure('ssh-connection-host-keys',sshText('主机公钥','Host public keys'),(c.hostFingerprints||[]).join('\n')+'\n'+c.hostKeys.join('\n'),state.sshConnectionHostKeysOpen)}`;
    buttons=sshButton('copy-command',sshText('复制 SSH 命令','Copy SSH command'),c.id,'primary',!c.tested);
  } else {title=sshText('状态已变化','Status changed');body=`<p>${sshText('请关闭窗口并重新选择设备。','Close this dialog and select the device again.')}</p>`;}
  return `<div class="modal-backdrop" role="presentation"><section class="modal ssh-modal" role="dialog" aria-modal="true" aria-labelledby="modal-title">${svg('shield','large')}<h2 id="modal-title">${title}</h2>${body}<div class="modal-actions"><button class="button" data-action="close-modal">${sshText('返回','Back')}</button>${buttons}</div></section></div>`;
}
async function sshCopy(text,successMessage='') {
  try {await navigator.clipboard.writeText(text);} catch (_) {
    const area=document.createElement('textarea');area.value=text;area.style.cssText='position:fixed;opacity:0';document.body.append(area);area.select();const copied=document.execCommand('copy');area.remove();if(!copied){showToast(sshText('复制失败，请在连接详情中手动复制','Copy failed. Copy manually from connection details.'));return;}
  }
  showToast(successMessage||sshText('连接信息已复制','Connection information copied'));
}
document.addEventListener('toggle',event=>{if(event.target.id==='ssh-host-keys')state.sshHostKeysOpen=event.target.open;if(event.target.id==='ssh-connection-host-keys')state.sshConnectionHostKeysOpen=event.target.open;},true);
document.addEventListener('click',event=>{
  const copyTarget=event.target.closest('[data-ssh-copy]');
  if(copyTarget){if(!copyTarget.disabled){const label=copyTarget.getAttribute('aria-label')||'';sshCopy(copyTarget.dataset.sshCopy||'',label===sshText('复制登录密码','Copy login password')?sshText('登录密码已复制','Login password copied'):'');}return;}
  const target=event.target.closest('[data-ssh]');if(!target||target.disabled)return;
  const action=target.dataset.ssh,id=target.dataset.id||'',peer=target.dataset.peer||'';
  if(action==='diagnostics'){sshCopy(sshDiagnostics());return;}
  if(action==='network'){state.modal={type:'ssh-network',id};render();return;}
  if(action==='select-out'||action==='select-in'||action==='go') {sshSelection={peer,incoming:action==='select-in'?id:''};navigate('remote');return;}
  const modals={local:'ssh-local',review:'ssh-review',waiting:'ssh-waiting','revoke-review':'ssh-revoke','host-review':'ssh-host',details:'ssh-details','grant-details':'ssh-grant-details'};
  if(modals[action]){if(action==='local')state.sshHostKeysOpen=false;if(action==='details')state.sshConnectionHostKeysOpen=false;state.modal={type:modals[action],id};render();return;}
  if(action==='setup-from-review'){state.sshReturnReview=id;state.modal={type:'ssh-local'};render();return;}
  if(action==='agent'||action==='copy-command') {
    const c=sshModel().data.outgoing.find(x=>x.id===id);if(!c?.tested||!['ready','authorized'].includes(c.status))return;
    sshCopy(action==='copy-command'?c.command:sshAgentInstructions(c),action==='agent'?sshText('已复制给 agent，粘贴到新会话即可','Copied for an agent. Paste it into a new session.'):'' );return;
  }
  const native={request:'ssh-request',approve:'ssh-approve',reject:'ssh-reject',cancel:'ssh-cancel',revoke:'ssh-revoke',setup:'ssh-setup','setup-client':'ssh-setup-client',refresh:'ssh-refresh',test:'ssh-test',terminal:'ssh-terminal',disconnect:'ssh-disconnect',trust:'ssh-trust'}[action];
  if(!native)return;
  const command={action:native};if(id)command.id=id;if(peer)command.fingerprint=peer;if(action==='approve')command.sid=state.modal?.sid;
  if(action==='trust')command.expected=state.modal?.expected;
  if(action==='request')state.sshWaitingPeer=peer;
  if(action==='test')state.sshPendingTest={id,previousTested:Number(sshModel().data.outgoing.find(x=>x.id===id)?.tested||0),started:false};
  state.sshSubmitting=action;
  if(!['setup','setup-client','refresh'].includes(action))state.modal=null;
  send(command);render();
});

function sshDiagnostics() {
  const s=sshModel(), safe={operation:s.operation,error:sshErrorText(s.error),listenerError:s.listenerError,localProxyError:s.localProxyError,remoteAccess:s.remoteAccess,server:{installed:s.server.installed,running:s.server.running,loopbackOnly:s.server.loopbackOnly,firewall:s.server.firewall,networks:s.server.networks},connections:s.data.outgoing.map(c=>({id:c.id,peer:c.peer,status:c.status,address:c.address,tested:c.tested,error:sshErrorText(c.error)}))};
  return JSON.stringify(safe,null,2);
}
