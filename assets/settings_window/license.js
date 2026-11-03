function licenseText(zh, en) { return currentModel().language === 'en' ? en : zh; }
function renderLicense() {
  const l = currentModel().license || {};
  const t = licenseText;
  const messages = {
    working: t('正在处理，请稍候…','Working…'), activated: t('激活成功。','Activation successful.'), verified: t('已联网验证。','License verified online.'),
    released: t('已解绑。请在新电脑使用原激活码。','Released. Use your original code on the new PC.'),
    bound_elsewhere: t('此码已绑定其他电脑。请先在旧电脑解绑；旧电脑无法使用时，请联系作者并提供购买凭证。','This code is bound to another PC. Release it there first, or contact the author with proof of purchase.'),
    unknown_code: t('激活码不存在或已停用，请核对原始激活码。','Unknown or disabled code. Check the original code.'),
    not_activated: t('当前电脑尚未激活。','This PC has not been activated.'), expired: t('离线验证期限已到，请联网验证，无需再次购买。','Offline verification has expired. Connect and verify; no additional purchase is needed.'),
    revoked: t('当前绑定已解除或停用，请联系作者核实。','This binding was released or disabled. Contact the author.'),
    release_pending: t('解绑尚未完成，请联网后点击“完成解绑”。','Release is pending. Connect and select Complete release.'),
    clock_error: t('系统时间异常，请校准日期和时间。','Correct your system date and time.'),
    rate_limited: t('操作频繁，请一分钟后再试。','Too many requests. Retry after one minute.'),
    invalid_local_data: t('本地授权文件损坏或不属于当前 Windows 用户，请联系作者。','Local license data is invalid or belongs to another Windows user. Contact the author.'),
    network_or_storage_error: t('暂时无法完成。请检查网络及本地存储权限；尚未到期的有效授权仍可使用。','Could not complete the request. Check the network and storage permissions. A valid offline license remains usable.')
  };
  const disabled = l.busy ? 'disabled' : '';
  const date = l.offlineUntil ? new Date(l.offlineUntil * 1000).toLocaleDateString(currentModel().language === 'en' ? 'en-US' : 'zh-CN') : '—';
  const remaining = Number(l.offlineUntil || 0) - Date.now() / 1000;
  const expiring = l.active && remaining > 0 && remaining <= 3 * 86400;
  // The status chip already says "not activated"; repeat only messages that add information.
  const message = l.message === 'not_activated' || (l.active && ['activated', 'verified'].includes(l.message)) ? '' : (messages[l.message] || '');
  const needsCode = !l.active && !l.pendingRelease;
  const [title, statusChip] = l.pendingRelease ? [t('永久授权','Perpetual license'), chip(t('解绑待完成','Release pending'), 'amber')]
    : l.active ? [t('永久授权','Perpetual license'), chip(t('已激活','Activated'), 'green')]
    : l.hasLicense ? [t('永久授权','Perpetual license'), chip(t('需要联网验证','Verification needed'), 'amber')]
    : [t('激活这台电脑','Activate this PC'), chip(t('未激活','Not activated'), 'amber')];
  const cases = [
    [t('旧电脑已损坏或重装系统','Old PC broken or Windows reinstalled'), t('凭购买凭证联系作者重置绑定','Contact the author with proof of purchase to reset the binding')],
    [t('更换了 Windows 用户','Switched Windows user'), t('可能需要重新激活','You may need to activate again')]
  ];
  return heading(t('激活与授权','Activation & license'), t('管理这台电脑的授权','Manage the license for this PC')) + `
    <article class="card license-card">
      <div class="license-head">
        <div class="license-identity">
          <div class="round-icon ${l.active && !l.pendingRelease ? 'green' : 'amber'}">${svg(l.active ? 'shield' : 'lock', 'large')}</div>
          <div class="license-title"><h2>${title}</h2>${statusChip}</div>
        </div>
        <div class="license-actions">
          <button class="button" data-license="refresh" ${disabled}>${svg('refresh')}${l.pendingRelease ? t('完成解绑','Complete release') : t('联网验证','Verify online')}</button>
          ${l.hasLicense ? `<button class="button danger" data-license="release" ${disabled} ${l.pendingRelease ? 'disabled' : ''}>${t('解绑并换机','Release for another PC')}</button>` : ''}
        </div>
      </div>
      ${message ? `<p class="license-notice" role="status" aria-live="polite">${escapeHtml(message)}</p>` : ''}
      ${expiring ? `<p class="license-notice amber" role="status">${t('离线使用期限即将到达，请联网验证，无需续费。','Your offline window ends soon. Verify online; no renewal payment is needed.')}</p>` : ''}
      ${needsCode ? `
      <div class="license-form">
        <label for="license-code">${t('激活码','Activation code')}</label>
        <div class="license-form-row">
          <input class="input" id="license-code" type="password" autocomplete="off" spellcheck="false" maxlength="64" placeholder="${t('粘贴购买时收到的激活码','Paste the activation code from your purchase')}" ${disabled}>
          <button class="button primary" data-license="activate" ${disabled}>${t('激活本机','Activate this PC')}</button>
        </div>
      </div>` : ''}
      ${l.hasLicense || l.licenseId ? `
      <div class="license-facts">
        <div class="license-fact"><span>${t('授权编号','License ID')}</span><strong class="mono">${escapeHtml(l.licenseId || '—')}</strong></div>
        <div class="license-fact"><span>${t('离线使用至','Offline access until')}</span><strong>${escapeHtml(date)}</strong></div>
      </div>` : ''}
      <p class="license-footnote muted small">${t('当前为测试授权，不能作为正式购买凭证。','This is a test license, not proof of a commercial purchase.')}</p>
    </article>
    <article class="card license-guide">
      <h2>${t('更换电脑','Move to another PC')}</h2>
      <ol class="license-steps">
        <li><span class="step-number">1</span><span>${t('准备好购买时收到的原激活码。','Keep the original activation code from your purchase at hand.')}</span></li>
        <li><span class="step-number">2</span><span>${t('在旧电脑点击“解绑并换机”。','On the old PC, select Release for another PC.')}</span></li>
        <li><span class="step-number">3</span><span>${t('在新电脑的“激活与授权”中输入原激活码。','On the new PC, enter the original code in Activation & license.')}</span></li>
      </ol>
      <div class="license-cases">
        <h3>${t('特殊情况','Special cases')}</h3>
        ${cases.map(([when, what]) => `<div class="license-case"><span>${when}</span><span>${what}</span></div>`).join('')}
      </div>
    </article>`;
}

document.addEventListener('click', event => {
  const button = event.target.closest('[data-license]');
  if (!button || button.disabled) return;
  const action = button.dataset.license;
  if (action === 'release') {
    state.modal = {type:'license-release'}; render(); return;
  }
  if (action === 'confirm-release') { state.modal = null; send({action:'license-release'}); render(); return; }
  if (action === 'activate') {
    const input = document.getElementById('license-code');
    const activation_code = (input?.value || '').trim().toLowerCase();
    if (!/^[a-f0-9]{64}$/.test(activation_code)) { showToast(licenseText('请输入完整的 64 位激活码。','Enter the complete 64-character code.')); return; }
    input.value = ''; input.blur();
    send({action:'license-activate', activation_code});
  } else if (action === 'refresh') send({action:'license-refresh'});
});
