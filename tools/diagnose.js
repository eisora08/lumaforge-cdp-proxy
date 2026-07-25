#!/usr/bin/env node
// Quick CDP diagnostic for LumaForge theme debugging
// Usage: node diagnose.js [port]

const http = require('http');
const WebSocket = require('ws');

const PORT = process.argv[2] || 50209;

function fetch(url) {
    return new Promise((resolve, reject) => {
        http.get(url, res => {
            let data = '';
            res.on('data', chunk => data += chunk);
            res.on('end', () => resolve(JSON.parse(data)));
        }).on('error', reject);
    });
}

async function evalInTarget(wsUrl, expression) {
    const ws = new WebSocket(wsUrl);
    let msgId = 0;
    const pending = new Map();

    ws.on('message', data => {
        const msg = JSON.parse(data.toString());
        if (msg.id && pending.has(msg.id)) {
            pending.get(msg.id)(msg);
            pending.delete(msg.id);
        }
    });

    function send(method, params = {}) {
        return new Promise(resolve => {
            const id = ++msgId;
            pending.set(id, resolve);
            ws.send(JSON.stringify({ id, method, params }));
        });
    }

    await new Promise(r => ws.on('open', r));
    await send('Runtime.enable');

    const result = await send('Runtime.evaluate', {
        expression,
        returnByValue: true,
        awaitPromise: false
    });

    // Wait briefly for console messages
    await new Promise(r => setTimeout(r, 500));
    ws.close();
    return result;
}

async function main() {
    const targets = await fetch(`http://127.0.0.1:${PORT}/json`);
    console.log('=== CDP Targets ===');
    for (const t of targets) {
        console.log(`  [${t.type}] "${t.title}" -> ${(t.url||'').substring(0, 90)}`);
    }

    const diag = `(function() {
        var r = {};
        r.hasOpener = !!window.opener;
        r.openerType = window.opener ? typeof window.opener : 'none';
        r.hasRouterHook = !!(window.opener||{}).__ROUTER_HOOK_INSTANCE;
        r.hasOwnRouterHook = !!window.__ROUTER_HOOK_INSTANCE;
        r.hasSidebar = !!document.querySelector('#sidebar');
        r.hasActiveIndicator = !!document.querySelector('.activeIndicator');
        r.hasSupernav = !!document.querySelector('[class*="supernav"]');
        r.hasAppConfig = !!document.querySelector('#application_config');
        r.has27qas = !!document.querySelector('[class*="_27qas"]');
        r.has1ENHE = !!document.querySelector('[class*="_1ENHE"]');
        r.has3mz8w = !!document.querySelector('[class*="_3mz8w"]');
        r.title = document.title;
        r.bodyChildCount = document.body ? document.body.children.length : 0;
        r.headChildCount = document.head ? document.head.children.length : 0;
        r.lumaforgeInjected = !!window.__lumaforge_theme_injected;
        // Find CSS link elements from lumaforge
        var links = document.querySelectorAll('link[data-lmf]');
        r.lmfCssLinks = Array.from(links).map(l => l.getAttribute('data-lmf').split('/').pop());
        // Find script elements from lumaforge  
        var scripts = document.querySelectorAll('script[data-lmf]');
        r.lmfJsScripts = Array.from(scripts).map(s => s.getAttribute('data-lmf').split('/').pop());
        // Find Steam obfuscated class patterns
        var sc = [];
        document.querySelectorAll('*').forEach(function(el) {
            if (el.className && typeof el.className === 'string' && el.className.match(/_[a-zA-Z0-9]{8,}/)) {
                sc.push(el.tagName + ' [' + el.className.substring(0, 100) + ']');
            }
        });
        r.steamElements = sc.slice(0, 20);
        return JSON.stringify(r, null, 2);
    })()`;

    // Target the "Steam" title window (SP Desktop)
    console.log('\n=== Checking "Steam" window (SP Desktop) ===');
    const steamTarget = targets.find(t => t.type === 'page' && t.title === 'Steam' && !t.title.includes('DevTools'));
    if (steamTarget) {
        const r = await evalInTarget(steamTarget.webSocketDebuggerUrl, diag);
        console.log(r.result?.result?.value || JSON.stringify(r, null, 2));
    } else {
        console.log('  Not found!');
    }

    // Also check SharedJSContext
    console.log('\n=== Checking SharedJSContext ===');
    const sjsc = targets.find(t => t.title === 'SharedJSContext');
    if (sjsc) {
        const r = await evalInTarget(sjsc.webSocketDebuggerUrl, `(function() {
            var r = {};
            r.hasOwnRouterHook = !!window.__ROUTER_HOOK_INSTANCE;
            r.routerHookKeys = Object.keys(window.__ROUTER_HOOK_INSTANCE || {});
            r.title = document.title;
            r.lumaforgeInjected = !!window.__lumaforge_theme_injected;
            return JSON.stringify(r, null, 2);
        })()`);
        console.log(r.result?.result?.value || JSON.stringify(r, null, 2));
    } else {
        console.log('  Not found!');
    }

    // Check Store Supernav
    console.log('\n=== Checking Store Supernav ===');
    const sn = targets.find(t => t.type === 'page' && t.title === 'Store Supernav');
    if (sn) {
        const r = await evalInTarget(sn.webSocketDebuggerUrl, `(function() {
            var r = {};
            r.hasRouterHook = !!window.__ROUTER_HOOK_INSTANCE;
            r.title = document.title;
            r.hasSidebar = !!document.querySelector('#sidebar');
            return JSON.stringify(r, null, 2);
        })()`);
        console.log(r.result?.result?.value || JSON.stringify(r, null, 2));
    }

    process.exit(0);
}

main().catch(e => { console.error(e); process.exit(1); });
