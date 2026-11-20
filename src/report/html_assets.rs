//! Inline stylesheet and script of the single-file HTML report (no network, no frameworks).
//!
//! Colors are role tokens: a categorical palette in fixed slot order (stages and series keep
//! their color across every chart), a one-hue sequential ramp for the heatmap, and status
//! colors reserved for verdicts and findings (always shown with an icon and a label).

pub const CSS: &str = r#"
:root{color-scheme:light;
--bg:#f6f6f4;--surface:#fcfcfb;--ink:#0b0b0b;--ink2:#52514e;--ink3:#77766f;--line:#e2e1dc;--grid:#ecebe7;
--s1:#2a78d6;--s2:#eb6834;--s3:#1baf7a;--s4:#eda100;--s5:#e87ba4;--s6:#008300;--s7:#6250d6;--s8:#e34948;--s0:#a8a7a0;
--q1:#cde2fb;--q2:#9ec5f4;--q3:#6da7ec;--q4:#3987e5;--q5:#256abf;--q6:#184f95;--q7:#0d366b;
--good:#0e7c3a;--good-bg:#e3f4e8;--info:#2a6fc0;--info-bg:#e5eefa;--warn:#9a5b00;--warn-bg:#fdf1d8;--crit:#b3261e;--crit-bg:#fbe4e2;
--code-bg:#efeeea;--accent:#2a78d6}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){color-scheme:dark;
--bg:#121211;--surface:#1a1a19;--ink:#f0efec;--ink2:#c3c2b7;--ink3:#97968d;--line:#33332f;--grid:#262624;
--s1:#3987e5;--s2:#d95926;--s3:#199e70;--s4:#c98500;--s5:#d55181;--s6:#0ca30c;--s7:#9085e9;--s8:#e66767;--s0:#6b6a64;
--q1:#16304f;--q2:#184f95;--q3:#256abf;--q4:#3987e5;--q5:#6da7ec;--q6:#9ec5f4;--q7:#cde2fb;
--good:#5fcf87;--good-bg:#173323;--info:#7fb0ef;--info-bg:#18263a;--warn:#f0b54a;--warn-bg:#3a2c12;--crit:#f08a82;--crit-bg:#3d1b19;
--code-bg:#262624;--accent:#6da7ec}}
:root[data-theme="dark"]{color-scheme:dark;
--bg:#121211;--surface:#1a1a19;--ink:#f0efec;--ink2:#c3c2b7;--ink3:#97968d;--line:#33332f;--grid:#262624;
--s1:#3987e5;--s2:#d95926;--s3:#199e70;--s4:#c98500;--s5:#d55181;--s6:#0ca30c;--s7:#9085e9;--s8:#e66767;--s0:#6b6a64;
--q1:#16304f;--q2:#184f95;--q3:#256abf;--q4:#3987e5;--q5:#6da7ec;--q6:#9ec5f4;--q7:#cde2fb;
--good:#5fcf87;--good-bg:#173323;--info:#7fb0ef;--info-bg:#18263a;--warn:#f0b54a;--warn-bg:#3a2c12;--crit:#f08a82;--crit-bg:#3d1b19;
--code-bg:#262624;--accent:#6da7ec}
*{box-sizing:border-box}
html{background:var(--bg)}
body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.5 system-ui,-apple-system,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;-webkit-text-size-adjust:100%}
code,pre,.mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,"Liberation Mono",monospace;font-size:12.5px}
code{background:var(--code-bg);padding:1px 5px;border-radius:4px;overflow-wrap:anywhere}
pre{background:var(--code-bg);padding:10px 12px;border-radius:6px;overflow:auto;max-height:340px}
a{color:var(--accent)}
.wrap{max-width:1120px;margin:0 auto;padding:0 16px}
header.top{display:flex;justify-content:space-between;align-items:flex-start;gap:16px;padding:24px 0 8px}
.brand{font-weight:700;letter-spacing:.02em;color:var(--ink3);font-size:12px;text-transform:uppercase}
h1{font-size:22px;margin:2px 0 4px;overflow-wrap:anywhere}
h2{font-size:17px;margin:0 0 12px}
h3{font-size:15px;margin:0 0 6px}
.meta{color:var(--ink2);font-size:13px}
nav.toc{position:sticky;top:0;z-index:5;background:var(--bg);border-bottom:1px solid var(--line);padding:8px 0;display:flex;gap:4px 14px;flex-wrap:wrap;font-size:13px}
nav.toc a{color:var(--ink2);text-decoration:none}
nav.toc a:hover{color:var(--ink)}
section{background:var(--surface);border:1px solid var(--line);border-radius:10px;padding:18px 18px 14px;margin:16px 0}
.sub{color:var(--ink2);font-size:13px;margin:-6px 0 12px}
.muted{color:var(--ink3)}
.verdict{display:flex;gap:12px;align-items:center;flex-wrap:wrap;margin-bottom:14px}
.pill{display:inline-flex;gap:6px;align-items:center;font-weight:700;font-size:12px;letter-spacing:.04em;padding:4px 10px;border-radius:999px}
.st-ok{color:var(--good);background:var(--good-bg)}.st-review{color:var(--info);background:var(--info-bg)}
.st-attention{color:var(--warn);background:var(--warn-bg)}.st-critical{color:var(--crit);background:var(--crit-bg)}
.headline{font-size:17px;font-weight:600;margin:0}
.tiles{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:10px;margin:8px 0 12px}
.tile{border:1px solid var(--line);border-radius:8px;padding:10px 12px}
.tile .k{color:var(--ink2);font-size:12px}.tile .v{font-size:20px;font-weight:650;font-variant-numeric:tabular-nums}.tile .s{color:var(--ink3);font-size:12px}
.notes p{margin:4px 0}
.finding{border:1px solid var(--line);border-left:4px solid var(--line);border-radius:8px;padding:12px 14px;margin:10px 0}
.finding.sev-critical{border-left-color:var(--crit)}.finding.sev-warning{border-left-color:var(--warn)}.finding.sev-info{border-left-color:var(--info)}
.finding header{display:flex;gap:10px;align-items:center;margin-bottom:4px;flex-wrap:wrap}
.finding .evidence{color:var(--ink2);margin:4px 0}
.finding .advice{margin:6px 0 0;font-weight:550}
.finding .verify{margin:6px 0 0;color:var(--ink2)}
dl.kv{display:grid;grid-template-columns:max-content 1fr;gap:3px 14px;margin:6px 0;font-size:13px}
dl.kv dt{color:var(--ink2)}dl.kv dd{margin:0;overflow-wrap:anywhere;font-variant-numeric:tabular-nums}
.grid2{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:12px 24px}
.tablewrap{overflow-x:auto;margin:6px 0}
table{border-collapse:collapse;width:100%;font-size:13px;font-variant-numeric:tabular-nums}
th,td{padding:5px 8px;border-bottom:1px solid var(--grid);text-align:right;white-space:nowrap}
th{color:var(--ink2);font-weight:600;border-bottom:1px solid var(--line);position:sticky;top:0;background:var(--surface)}
th:first-child,td:first-child,td.l,th.l{text-align:left}
td.wrap{white-space:normal;overflow-wrap:anywhere;min-width:160px}
tr.bad td{color:var(--crit)}
th.sort{cursor:pointer;user-select:none}th.sort:hover{color:var(--ink)}
th[aria-sort="ascending"]::after{content:" ▲";font-size:10px}th[aria-sort="descending"]::after{content:" ▼";font-size:10px}
.legend{display:flex;flex-wrap:wrap;gap:4px 14px;font-size:12.5px;color:var(--ink2);margin:4px 0 6px}
.legend i{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:5px;vertical-align:-1px}
.chart{margin:4px 0 14px}
.chart h3{font-size:13.5px;margin:0 0 2px}
svg.viz{width:100%;height:auto;display:block;overflow:visible}
svg.viz text{fill:var(--ink2);font-size:11px;font-family:inherit}
svg.viz .ax{stroke:var(--line)}svg.viz .gr{stroke:var(--grid)}
svg.viz .ln{fill:none;stroke-width:2;stroke-linejoin:round}
svg.viz .hit rect{fill:transparent}svg.viz .hit .xh{stroke:var(--ink3);stroke-width:1;opacity:0}
svg.viz .hit:hover .xh{opacity:.8}svg.viz .hit:hover rect{fill:var(--ink);fill-opacity:.04}
svg.viz .seg:hover,svg.viz .cell:hover{stroke:var(--ink);stroke-width:1}
.controls{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:4px 0 8px}
.controls input,.controls select,.controls button,button.btn{font:inherit;font-size:13px;padding:5px 9px;border:1px solid var(--line);border-radius:6px;background:var(--surface);color:var(--ink)}
.controls input{min-width:260px;flex:1}
button.btn{cursor:pointer}button.btn:hover{border-color:var(--ink3)}
#theme{background:var(--surface);border:1px solid var(--line);color:var(--ink);border-radius:8px;padding:6px 10px;cursor:pointer;font:inherit}
details summary{cursor:pointer;color:var(--ink2)}
footer{color:var(--ink3);font-size:12px;padding:8px 0 32px}
.swatch{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:6px;vertical-align:-1px}
.bars .row{display:grid;grid-template-columns:150px 1fr 90px;gap:10px;align-items:center;margin:3px 0;font-size:13px}
.bars .track{height:12px;background:var(--grid);border-radius:3px;overflow:hidden}
.bars .fill{height:100%;background:var(--s1);border-radius:0 4px 4px 0}
.bars .row.sel{font-weight:650}
.bars .num{text-align:right;font-variant-numeric:tabular-nums}
@media (max-width:640px){.bars .row{grid-template-columns:100px 1fr 70px}.controls input{min-width:0}}
@media print{
:root,:root[data-theme="dark"]{color-scheme:light;--bg:#fff;--surface:#fff;--ink:#000;--ink2:#333;--ink3:#555;--line:#ccc;--grid:#e6e6e6;--code-bg:#f2f2f2;
--s1:#2a78d6;--s2:#eb6834;--s3:#1baf7a;--s4:#eda100;--s5:#e87ba4;--s6:#008300;--s7:#6250d6;--s8:#e34948;
--q1:#cde2fb;--q2:#9ec5f4;--q3:#6da7ec;--q4:#3987e5;--q5:#256abf;--q6:#184f95;--q7:#0d366b}
.noprint,nav.toc,.controls,#more{display:none!important}
section{break-inside:avoid-page;border-color:#ccc}
.chart,.finding,table{break-inside:avoid}
th{position:static}
pre{max-height:none}
}
"#;

pub const JS: &str = r##"
(function(){
var root=document.documentElement;
try{var saved=localStorage.getItem('oprobe-theme');if(saved==='light'||saved==='dark'){root.setAttribute('data-theme',saved);}}catch(e){}
var tb=document.getElementById('theme');
if(tb){tb.addEventListener('click',function(){
var cur=root.getAttribute('data-theme');
var dark=cur?cur==='dark':(window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches);
var next=dark?'light':'dark';root.setAttribute('data-theme',next);
try{localStorage.setItem('oprobe-theme',next);}catch(e){}});}
var el=document.getElementById('oprobe-rows');if(!el){return;}
var D;try{D=JSON.parse(el.textContent);}catch(e){return;}
var C=D.cols,ROWS=D.rows,I={};C.forEach(function(c,i){I[c]=i;});
var body=document.getElementById('req-body'),count=document.getElementById('req-count'),
more=document.getElementById('more'),q=document.getElementById('req-q'),kind=document.getElementById('req-kind');
var state={key:'t_ms',dir:1,shown:200};
function esc(s){return String(s).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];});}
function ms(v){if(v===null||v===undefined||v==='')return '';v=+v;if(v>=1000)return (v/1000).toFixed(2)+'s';if(v>=100)return v.toFixed(0)+'ms';if(v>=10)return v.toFixed(1)+'ms';return v.toFixed(2)+'ms';}
function by(v){v=+v;if(v>=1073741824)return (v/1073741824).toFixed(2)+' GiB';if(v>=1048576)return (v/1048576).toFixed(1)+' MiB';if(v>=1024)return (v/1024).toFixed(1)+' KiB';return v+' B';}
function isErr(r){var s=r[I.status];return s===0||s>=400;}
function isThr(r){var s=r[I.status];return s===429||s===503;}
function test(r){
var k=kind?kind.value:'all';
if(k==='errors'&&!isErr(r))return false;
if(k==='throttled'&&!isThr(r))return false;
if(k==='cold'&&(r[I.reused]||r[I.proto]==='inproc'))return false;
if(k==='retries'&&r[I.attempt]<2)return false;
var text=q?q.value.trim().toLowerCase():'';if(!text)return true;
var toks=text.split(/\s+/);
for(var t=0;t<toks.length;t++){var tok=toks[t],m=tok.match(/^([a-z_]+)(:|>=|<=|>|<|=)(.+)$/);
if(m&&I.hasOwnProperty(m[1])){var v=r[I[m[1]]],w=m[3],op=m[2];
if(op===':'){if(String(v).toLowerCase().indexOf(w)<0)return false;}
else if(op==='='){if(String(v).toLowerCase()!==w)return false;}
else{var a=+v,b=+w;if(isNaN(a)||isNaN(b))return false;
if(op==='>'&&!(a>b))return false;if(op==='<'&&!(a<b))return false;if(op==='>='&&!(a>=b))return false;if(op==='<='&&!(a<=b))return false;}
continue;}
var hit=false;for(var j=0;j<r.length;j++){if(String(r[j]).toLowerCase().indexOf(tok)>=0){hit=true;break;}}
if(!hit)return false;}
return true;}
function draw(){
var rows=ROWS.filter(test),key=I[state.key],dir=state.dir;
rows.sort(function(a,b){var x=a[key],y=b[key];if(x===y)return a[I.t_ms]-b[I.t_ms];return (x<y?-1:1)*dir;});
var html=[],n=Math.min(rows.length,state.shown);
for(var i=0;i<n;i++){var r=rows[i],conn=r[I.proto]==='inproc'?'in-process':r[I.proto]+' #'+r[I.conn]+(r[I.reused]?' warm':' cold');
html.push('<tr'+(isErr(r)?' class="bad"':'')+'><td>'+(r[I.t_ms]/1000).toFixed(3)+'</td><td><a href="#" data-op="'+r[I.op]+'">'+r[I.op]+'</a></td><td>'+r[I.attempt]+'</td><td class="l">'+esc(r[I.api])+'</td><td>'+(r[I.status]||'ERR')+'</td><td class="l">'+esc(conn)+'</td><td class="l">'+esc(r[I.remote])+'</td><td>'+ms(r[I.queue_ms])+'</td><td>'+(r[I.connect_ms]?ms(r[I.connect_ms]):'')+'</td><td>'+ms(r[I.send_ms])+'</td><td>'+ms(r[I.ttfb_ms])+'</td><td>'+ms(r[I.body_ms])+'</td><td><b>'+ms(r[I.total_ms])+'</b></td><td>'+by(r[I.up])+'</td><td>'+by(r[I.down])+'</td><td class="l wrap">'+esc(r[I.error]||'')+(r[I.req_id]?' <span class="muted">'+esc(r[I.req_id])+'</span>':'')+'</td></tr>');}
body.innerHTML=html.join('');
count.textContent=rows.length===ROWS.length?(ROWS.length+' rows'):(rows.length+' of '+ROWS.length+' rows match');
more.style.display=rows.length>n?'':'none';}
document.querySelectorAll('#req-table th[data-k]').forEach(function(th){th.classList.add('sort');th.setAttribute('tabindex','0');
function go(){var k=th.getAttribute('data-k');if(state.key===k){state.dir=-state.dir;}else{state.key=k;state.dir=(k==='t_ms'||k==='op'||k==='api')?1:-1;}
document.querySelectorAll('#req-table th[data-k]').forEach(function(o){o.removeAttribute('aria-sort');});
th.setAttribute('aria-sort',state.dir>0?'ascending':'descending');state.shown=200;draw();}
th.addEventListener('click',go);th.addEventListener('keydown',function(e){if(e.key==='Enter'||e.key===' '){e.preventDefault();go();}});});
if(q){q.addEventListener('input',function(){state.shown=200;draw();});}
if(kind){kind.addEventListener('change',function(){state.shown=200;draw();});}
if(more){more.addEventListener('click',function(){state.shown+=500;draw();});}
document.addEventListener('click',function(e){var a=e.target.closest?e.target.closest('[data-op]'):null;if(!a||!q)return;e.preventDefault();
q.value='op='+a.getAttribute('data-op');if(kind)kind.value='all';state.shown=200;state.key='t_ms';state.dir=1;draw();
document.getElementById('requests').scrollIntoView({behavior:'smooth'});});
draw();
})();
"##;
