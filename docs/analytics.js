/* Google Analytics 4 for anacraft.dev, behind Consent Mode v2.
 *
 * Loaded synchronously from <head> on every page, because the consent defaults
 * below must execute before gtag.js does — a tag that loads first has already
 * written its cookie by the time we deny storage.
 *
 * Nothing is stored on a visitor's machine until they accept. Until then GA4
 * sends cookieless pings, which is what keeps this compatible with the claim
 * the privacy policy makes.
 *
 * This covers the website only. The anacraft binary has no telemetry of any
 * kind and never phones home; do not let this file blur that line.
 */
(function () {
  var ID = 'G-DYPCVZVMSE';
  var KEY = 'anacraft-consent';

  window.dataLayer = window.dataLayer || [];
  function gtag() { dataLayer.push(arguments); }
  window.gtag = gtag;

  /* localStorage throws outright in Safari's private mode rather than
     returning null, so every access is guarded. */
  var choice = null;
  try { choice = localStorage.getItem(KEY); } catch (e) {}

  /* A browser sending Global Privacy Control has already answered this
     question at the browser level; asking again with a banner would be
     ignoring it. Treat it as a decline and never show the prompt. */
  if (navigator.globalPrivacyControl === true) choice = 'denied';

  gtag('consent', 'default', {
    ad_storage: 'denied',
    ad_user_data: 'denied',
    ad_personalization: 'denied',
    analytics_storage: 'denied',
    wait_for_update: 500
  });

  if (choice === 'granted') {
    gtag('consent', 'update', { analytics_storage: 'granted' });
  }

  gtag('js', new Date());
  gtag('config', ID);

  var tag = document.createElement('script');
  tag.async = true;
  tag.src = 'https://www.googletagmanager.com/gtag/js?id=' + ID;
  document.head.appendChild(tag);

  if (choice === 'granted' || choice === 'denied') return;

  function decide(value) {
    try { localStorage.setItem(KEY, value); } catch (e) {}
    if (value === 'granted') gtag('consent', 'update', { analytics_storage: 'granted' });
    var el = document.getElementById('consent-bar');
    if (el) el.parentNode.removeChild(el);
  }

  function banner() {
    var css = document.createElement('style');
    css.textContent = [
      '#consent-bar{position:fixed;left:0;right:0;bottom:0;z-index:200;',
      'background:var(--panel,#0d1512);border-top:1px solid var(--line-2,rgba(193,196,151,.20));',
      'padding:14px 24px;display:flex;flex-wrap:wrap;gap:14px 22px;align-items:center;',
      "font-family:'Inter',-apple-system,BlinkMacSystemFont,sans-serif;font-size:13.5px;",
      'color:var(--fg,#c1c497);line-height:1.55}',
      '#consent-bar p{margin:0;max-width:70ch}',
      '#consent-bar a{color:var(--jade,#2dd5b7);border-bottom:1px solid rgba(45,213,183,.3);text-decoration:none}',
      '#consent-bar .btns{margin-left:auto;display:flex;gap:10px;flex-shrink:0}',
      '#consent-bar button{font-family:var(--mono,ui-monospace,Menlo,monospace);font-size:12px;',
      'letter-spacing:.06em;text-transform:uppercase;padding:8px 16px;cursor:pointer;',
      'background:transparent;color:var(--fg,#c1c497);',
      'border:1px solid var(--line-2,rgba(193,196,151,.20));border-radius:0;transition:all .15s}',
      '#consent-bar button:hover{color:var(--white,#eef1dc);border-color:var(--jade,#2dd5b7)}',
      '#consent-bar button.yes{background:var(--jade,#2dd5b7);border-color:var(--jade,#2dd5b7);color:#04120e;font-weight:700}',
      '#consent-bar button.yes:hover{opacity:.88;color:#04120e}',
      '@media(max-width:560px){#consent-bar{padding:14px 18px}#consent-bar .btns{margin-left:0;width:100%}',
      '#consent-bar button{flex:1}}'
    ].join('');
    document.head.appendChild(css);

    var bar = document.createElement('div');
    bar.id = 'consent-bar';
    bar.setAttribute('role', 'dialog');
    bar.setAttribute('aria-label', 'Analytics consent');
    bar.innerHTML =
      '<p>anacraft.dev counts visits with Google Analytics. <b>Nothing is stored on your ' +
      'device unless you accept</b> — decline and the site keeps working exactly the same. ' +
      '<a href="/privacy.html">Privacy policy</a></p>' +
      '<div class="btns">' +
      '<button type="button" data-choice="denied">Decline</button>' +
      '<button type="button" class="yes" data-choice="granted">Accept</button>' +
      '</div>';
    bar.addEventListener('click', function (e) {
      var b = e.target.closest('button[data-choice]');
      if (b) decide(b.getAttribute('data-choice'));
    });
    document.body.appendChild(bar);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', banner);
  } else {
    banner();
  }
})();
