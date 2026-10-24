# Write TERRAFOLD game to C:\Users\Asus\terrafold\index.html
import os

HTML = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>TERRAFOLD — terraforming god game</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
html,body{width:100%;height:100%;overflow:hidden;background:#0a0a0f;color:#c8c8d0;user-select:none;font-family:system-ui,sans-serif}
canvas{display:block;cursor:crosshair}
#hud{position:fixed;bottom:0;left:0;right:0;height:62px;background:linear-gradient(0deg,rgba(10,10,18,.95),rgba(10,10,18,.6) 60%,transparent);display:flex;align-items:flex-end;justify-content:center;padding:0 12px 8px;gap:3px;pointer-events:none}
#hud>*{pointer-events:auto}
.tool-btn{width:44px;height:44px;border-radius:10px;border:1px solid rgba(255,255,255,.08);background:rgba(255,255,255,.04);color:#888;font-size:18px;cursor:pointer;transition:all .15s;display:flex;align-items:center;justify-content:center;position:relative}
.tool-btn:hover{background:rgba(255,255,255,.1);color:#ddd}
.tool-btn.active{background:rgba(100,180,255,.2);border-color:#6cf;color:#6cf;box-shadow:0 0 12px rgba(100,180,255,.15)}
.tool-btn .lbl{position:absolute;bottom:-13px;font-size:8px;color:#555;white-space:nowrap;letter-spacing:.3px}
.tool-btn.active .lbl{color:#6cf}
#stats{position:fixed;top:10px;right:14px;font-size:10px;color:rgba(200,200,210,.4);font-variant-numeric:tabular-nums;text-align:right;pointer-events:none;line-height:1.6}
#stats .v{color:rgba(200,200,210,.7);min-width:42px;display:inline-block;text-align:right}
#title{position:fixed;top:12px;left:50%;transform:translateX(-50%);font-size:9px;letter-spacing:5px;text-transform:uppercase;color:rgba(200,200,210,.15);font-weight:600;pointer-events:none}
#info{position:fixed;bottom:72px;left:50%;transform:translateX(-50%);font-size:10px;color:rgba(200,200,210,.2);text-align:center;pointer-events:none;letter-spacing:.5px}
#sbar{position:fixed;bottom:74px;right:16px;display:flex;align-items:center;gap:6px;pointer-events:none}
#sbar>*{pointer-events:auto}
#sbar label{font-size:8px;color:#555;text-transform:uppercase;letter-spacing:1px}
#sbar input{width:72px;accent-color:#6cf;height:3px;cursor:pointer;opacity:.6}
#sbar input:hover{opacity:1}
#zbar{position:fixed;bottom:74px;left:16px;display:flex;gap:4px;pointer-events:none}
#zbar>*{pointer-events:auto}
#zbar button{width:26px;height:26px;border-radius:6px;border:1px solid rgba(255,255,255,.08);background:rgba(255,255,255,.04);color:#888;cursor:pointer;font-size:13px;line-height:26px;text-align:center}
#zbar button:hover{background:rgba(255,255,255,.1);color:#ddd}
#reset{position:fixed;top:10px;left:14px;border:none;background:none;color:rgba(200,200,210,.15);font-size:8px;letter-spacing:1px;cursor:pointer;text-transform:uppercase;transition:color .2s}
#reset:hover{color:rgba(200,200,210,.4)}
@media(max-width:640px){
#stats{font-size:8px;top:8px;right:8px}
.tool-btn{width:34px;height:34px;font-size:14px}
.tool-btn .lbl{display:none}
#hud{height:48px;padding:0 4px 4px;gap:2px}
#sbar{bottom:58px;right:8px}#sbar input{width:50px}
#zbar{display:none}
#title{font-size:7px}
#info{bottom:58px;font-size:8px}
}
</style>
</head>
<body>
<canvas id="c"></canvas>
<button id="reset">⟳ New World</button>
<div id="title">TERRAFOLD</div>
<div id="stats">ELEV <span class="v" id="se">-</span><br>WATER <span class="v" id="sw">-</span><br>FOREST <span class="v" id="sf">-</span><br>SCORE <span class="v" id="ss">-</span></div>
<div id="zbar"><button id="zo">−</button><button id="zi">+</button></div>
<div id="sbar"><label>Size</label><input type="range" id="str" min="1" max="10" value="4"></div>
<div id="info">Click & drag</div>
<div id="hud">
<button class="tool-btn active" data-t="raise"><span>🏔</span><span class="lbl">Raise</span></button>
<button class="tool-btn" data-t="lower"><span>⬇</span><span class="lbl">Lower</span></button>
<button class="tool-btn" data-t="water"><span>💧</span><span class="lbl">Water</span></button>
<button class="tool-btn" data-t="forest"><span>🌳</span><span class="lbl">Forest</span></button>
<button class="tool-btn" data-t="erode"><span>🌊</span><span class="lbl">Erode</span></button>
<button class="tool-btn" data-t="smooth"><span>🌀</span><span class="lbl">Smooth</span></button>
<button class="tool-btn" data-t="flat"><span>▤</span><span class="lbl">Flatten</span></button>
<button class="tool-btn" data-t="crater"><span>🌋</span><span class="lbl">Crater</span></button>
</div>
<script>
// TERRAFOLD - procedural terraforming sandbox
// Perlin noise
let P=new Uint8Array(512);P.set(Uint8Array.from({length:256},(_,i)=>i));
for(let i=255;i>0;i--){let j=Math.random()*i|0;[P[i],P[j]]=[P[j],P[i]]}P.set(P.subarray(0,256),256);
function PN(x,y){let ix=x|0,iy=y|0,fx=x-ix,fy=y-iy,a=P[P[ix&255]+(iy&255)],b=P[P[ix&255]+(iy+1&255)],c=P[P[ix+1&255]+(iy&255)],d=P[P[ix+1&255]+(iy+1&255)];let F=t=>t*t*t*(t*(t*6-15)+10),L=(a,b,t)=>a+t*(b-a);return L(L(a,c,F(fx)),L(b,d,F(fx)),F(fy))/128-1}
function FBM(x,y,o){let oc=o?.octaves||6,la=o?.lac||2,ga=o?.gain||.5,sc=o?.sc||.02;let v=0,amp=1,fr=1,mx=0;for(let i=0;i<oc;i++){v+=amp*PN(x*fr*sc,y*fr*sc);mx+=amp;amp*=ga;fr*=la}return v/mx}
// World
const S=200;let h=new Float32Array(S*S),w=new Float32Array(S*S),f=new Float32Array(S*S),t=new Float32Array(S*S),fl=new Float32Array(S*S);
function I(x,y){return((y+S)%S)*S+((x+S)%S)}
function GH(x,y){return h[I(x,y)]}
function AD(x,y,z){let i=I(x,y);h[i]=Math.max(-1,Math.min(1,h[i]+z))}
function GEN(){for(let y=0;y<S;y++)for(let x=0;x<S;x++){let cx=x/S-.5,cy=y/S-.5,d=Math.sqrt(cx*cx+cy*cy),is=1-Math.pow(d*1.6,1.5),b=FBM(x,y,{sc:.025,octaves:7})*.7+FBM(x,y,{sc:.06,octaves:4})*.3,rd=Math.max(0,1-Math.abs(PN(x*.012,y*.012))*1.2)*.4,hh=b*.6+rd-.1,m=Math.max(0,is)*.9+.1;h[I(x,y)]=(hh*m+.1)*1.1}for(let i=0;i<8;i++){let px=Math.random()*S|0,py=Math.random()*S|0;for(let dy=-12;dy<=12;dy++)for(let dx=-12;dx<=12;dx++){let d=Math.sqrt(dx*dx+dy*dy);if(d<12)AD(px+dx,py+dy,.25*Math.cos(d/12*Math.PI/2))}}for(let y=0;y<S;y++)for(let x=0;x<S;x++){let la=Math.abs(y/S-.5)*2,n=FBM(x,y,{sc:.03,octaves:3})*.15;t[I(x,y)]=Math.max(0,Math.min(1,1-la*.9+n))}for(let y=0;y<S;y++)for(let x=0;x<S;x++){if(h[I(x,y)]<-0.15)w[I(x,y)]=.3}}
GEN();function RESET(){h.fill(0);w.fill(0);f.fill(0);GEN()}
function FLOW(){fl.fill(0);for(let y=0;y<S;y++)for(let x=0;x<S;x++){if(h[I(x,y)]<0)continue;let mh=GH(x,y);for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++){if(!dx&&!dy)continue;let nh=GH(x+dx,y+dy);if(nh<mh)mh=nh}if(mh<GH(x,y))fl[I(x,y)]=1}for(let it=0;it<5;it++)for(let y=S-1;y>=0;y--)for(let x=S-1;x>=0;x--){let i=I(x,y);if(h[i]<0||fl[i]<.5)continue;let mh=GH(x,y),bx=x,by=y;for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++){if(!dx&&!dy)continue;let nh=GH(x+dx,y+dy);if(nh<mh){mh=nh;bx=x+dx;by=y+dy}}if(bx!==x||by!==y)fl[I(bx,by)]+=fl[i]*.1}for(let i=0;i<S*S;i++)fl[i]=Math.min(fl[i],20)}
function ERODE(p,s){p=p||8;s=s||.02;for(let p2=0;p2<p;p2++)for(let y=1;y<S-1;y++)for(let x=1;x<S-1;x++){let i=I(x,y);if(h[i]<-.1)continue;let sl=(Math.abs(GH(x+1,y)-h[i])+Math.abs(GH(x-1,y)-h[i])+Math.abs(GH(x,y+1)-h[i])+Math.abs(GH(x,y-1)-h[i]))/4,sd=sl*(fl[i]/20)*s;if(sd>.001){h[i]-=sd;let mh=h[i],bx=x,by=y;for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++){if(!dx&&!dy)continue;let nh=GH(x+dx,y+dy);if(nh<mh){mh=nh;bx=x+dx;by=y+dy}}if(bx!==x||by!==y)h[I(bx,by)]+=sd*.5}}}
function GROW(p){p=p||4;for(let p2=0;p2<p;p2++){for(let y=1;y<S-1;y++)for(let x=1;x<S-1;x++){let i=I(x,y);if(h[i]<-.05||w[i]>.1)continue;let m=fl[i]/20+w[i]*.5,hb=Math.max(0,1-Math.abs(h[i])*1.5),g=Math.max(0,t[i]-.2)*m*hb*2,sp=(f[I(x+1,y)]+f[I(x-1,y)]+f[I(x,y+1)]+f[I(x,y-1)])/4*.05;f[i]=Math.min(1,Math.max(0,f[i]+g*.06+sp-.003))}for(let i=0;i<S*S;i++){if(h[i]>.5||h[i]<-.2||t[i]<.05||t[i]>.95)f[i]*=.95}}}
function SPREAD(p){p=p||4;for(let p2=0;p2<p;p2++){let nw=new Float32Array(w);for(let y=1;y<S-1;y++)for(let x=1;x<S-1;x++){let i=I(x,y);if(h[i]>.2){nw[i]*=.98;continue}let s=0,c=0;for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++){if(!dx&&!dy)continue;let ni=I(x+dx,y+dy);if(h[ni]<h[i]+w[i]*.2+.01){s+=w[ni];c++}}if(c>0)nw[i]+=(s/c-w[i])*.15;if(t[i]>.7)nw[i]*=.998}w.set(nw);for(let i=0;i<S*S;i++)w[i]=Math.max(0,Math.min(1,w[i]))}}
// Renderer
let cn=document.getElementById('c'),cx=cn.getContext('2d'),oc=document.createElement('canvas'),ox=oc.getContext('2d');
let ZO=1,PX=0,PY=0,TM=0,ST=null,WW,HH;
function RS(){let d=Math.min(window.devicePixelRatio||1,2);WW=window.innerWidth;HH=window.innerHeight;cn.width=WW*d;cn.height=HH*d;cn.style.width=WW+'px';cn.style.height=HH+'px';cx.setTransform(1,0,0,1,0,0);cx.scale(d,d);oc.width=WW;oc.height=HH;ST=null}
RS();
function RENDER(){TM+=.016;let cs=Math.max(2,Math.min(10,ZO*6)),m=16,ox2=PX+m,oy2=PY+m,gx0=Math.max(0,(-ox2/cs)|0),gy0=Math.max(0,(-oy2/cs)|0),gx1=Math.min(S,((WW-ox2)/cs)|0+1),gy1=Math.min(S,((HH-oy2)/cs)|0+1);let lx=Math.cos(TM*.03)*.4+.4,ly=Math.sin(TM*.03)*.4+.4,amb=.3+.2*Math.max(0,Math.sin(TM*.02));let bio=[[-.35,'#1a2a4a'],[-.15,'#2a5a7a'],[-.05,'#8aa8a0'],[0,'#5a8a4a'],[.15,'#4a7a3a'],[.3,'#3a6a2a'],[.45,'#6a5a3a'],[.6,'#8a7a5a'],[.8,'#9a9aaa'],[1,'#d0d0e0']];let bg=ox.createRadialGradient(WW/2,HH/2,0,WW/2,HH/2,WW);bg.addColorStop(0,'#0d111f');bg.addColorStop(.5,'#0a0a12');bg.addColorStop(1,'#06060a');ox.fillStyle=bg;ox.fillRect(0,0,WW,HH);let sx=WW/2+Math.cos(TM*.02)*WW*.3,sy=HH/4+Math.sin(TM*.02)*HH*.15,sa=.5+.3*Math.sin(TM*.02);let ag=ox.createRadialGradient(sx,sy,0,sx,sy,WW*.6);ag.addColorStop(0,'rgba(255,200,100,'+(.02+sa*.02)+')');ag.addColorStop(.5,'rgba(200,150,80,.01)');ag.addColorStop(1,'transparent');ox.fillStyle=ag;ox.fillRect(0,0,WW,HH);for(let gy=gy0;gy<gy1;gy++)for(let gx=gx0;gx<gx1;gx++){let i=I(gx,gy),hh=h[i],wd=w[i],ft=f[i],f2=fl[i]/20,col=[30,40,60];for(let b=0;b<bio.length-1;b++){if(hh>=bio[b][0]&&hh<bio[b+1][0]){let t2=(hh-bio[b][0])/(bio[b+1][0]-bio[b][0]),c1=PH(bio[b][1]),c2=PH(bio[b+1][1]);col=c1.map((v,j)=>v+(c2[j]-v)*t2);break}}if(hh>=bio[bio.length-1][0])col=PH(bio[bio.length-1][1]);let tmp=t[i];col[0]+=(tmp-.5)*6;col[1]+=(tmp-.5)*3;let hL=GH(gx-1,gy)||hh,hR=GH(gx+1,gy)||hh,hU=GH(gx,gy-1)||hh,hD=GH(gx,gy+1)||hh;let dx=(hL-hR)*6,dy=(hU-hD)*6,len=Math.sqrt(dx*dx+dy*dy+1),diff=Math.max(0,(dx*lx+dy*ly+.8)/len),light=Math.min(1,amb+diff*.6);col=col.map(v=>v*light);if(wd>.01){let wv=Math.sin(gx*.3+gy*.2+TM*2)*.03,wl=Math.min(1,wd+wv+.3);col[0]=col[0]*(1-wl*2)+70*wl*2;col[1]=col[1]*(1-wl*2)+110*wl*2;col[2]=col[2]*(1-wl*2)+170*wl*2}if(ft>.05){col[0]-=ft*15;col[1]+=ft*15;col[2]-=ft*3}if(f2>.05){col[0]+=8;col[1]+=12;col[2]+=25}if(hh>.55&&tmp<.35){let sn=(hh-.55)/.3;col=col.map((v,idx)=>v+(sn*(255-v))*[.5,.6,.9][idx])}let px2=ox2+gx*cs|0,py2=oy2+gy*cs|0;ox.fillStyle='rgb('+(col[0]|0)+','+(col[1]|0)+','+(col[2]|0)+')';ox.fillRect(px2,py2,cs,cs);if(cs>=3){ox.fillStyle='rgba(255,255,255,.02)';ox.fillRect(px2,py2,1,cs);ox.fillRect(px2,py2,cs,1)}}let hz=ox.createLinearGradient(0,HH*.7,0,HH);hz.addColorStop(0,'transparent');hz.addColorStop(1,'rgba(10,10,18,.3)');ox.fillStyle=hz;ox.fillRect(0,HH*.7,WW,HH*.3);if(!ST){ST=Array.from({length:50},()=>[Math.random()*WW|0,Math.random()*HH|0,Math.random()*.5+.3])}for(let s of ST){ox.fillStyle='rgba(255,255,255,'+(s[2]*.12)+')';ox.fillRect(s[0],s[1],1,1)}cx.clearRect(0,0,WW,HH);cx.drawImage(oc,0,0)}
function PH(hx){return[parseInt(hx.slice(1,3),16),parseInt(hx.slice(3,5),16),parseInt(hx.slice(5,7),16)]}
function S2W(sx,sy){let cs=Math.max(2,Math.min(10,ZO*6)),ox2=PX+16,oy2=PY+16,gx=((sx-ox2)/cs)|0,gy=((sy-oy2)/cs)|0;if(gx<0||gx>=S||gy<0||gy>=S)return null;return{x:gx,y:gy}}
// App
let AT='raise',BS=4,MD=false,LLX=-1,LLY=-1;
document.querySelectorAll('.tool-btn').forEach(b=>{b.addEventListener('click',()=>{document.querySelectorAll('.tool-btn').forEach(b2=>b2.classList.remove('active'));b.classList.add('active');AT=b.dataset.t;let ms={raise:'Raise - build mountains',lower:'Lower - carve valleys',water:'Add water - fill lakes',forest:'Plant forest - trees',erode:'Erode - weather the land',smooth:'Smooth - soften edges',flat:'Flatten - level area',crater:'Crater - single blast'};document.getElementById('info').textContent=ms[AT]||''})});
document.getElementById('str').addEventListener('input',function(){BS=+this.value});
document.getElementById('zi').addEventListener('click',()=>{ZO=Math.min(3,ZO+.2)});
document.getElementById('zo').addEventListener('click',()=>{ZO=Math.max(.3,ZO-.2)});
document.getElementById('reset').addEventListener('click',()=>{RESET();ST=null;FU()});
function PT(x,y){let p=S2W(x,y);if(!p)return;let gx=p.x,gy=p.y,R=BS;for(let dy=-R;dy<=R;dy++)for(let dx=-R;dx<=R;dx++){let d=Math.sqrt(dx*dx+dy*dy);if(d>R)continue;let ff=1-d/(R+1),xx=gx+dx,yy=gy+dy;if(xx<0||xx>=S||yy<0||yy>=S)continue;switch(AT){case'raise':AD(xx,yy,.05*ff);break;case'lower':AD(xx,yy,-.05*ff);break;case'water':if(h[I(xx,yy)]<.2)w[I(xx,yy)]=Math.min(1,w[I(xx,yy)]+.07*ff);break;case'forest':if(h[I(xx,yy)]>-.05&&h[I(xx,yy)]<.5)f[I(xx,yy)]=Math.min(1,f[I(xx,yy)]+.08*ff);break;case'erode':{let i=I(xx,yy);h[i]-=.015*ff;let mh=h[i],bx=xx,by=yy;for(let d2=-1;d2<=1;d2++)for(let d1=-1;d1<=1;d1++){if(!d1&&!d2)continue;let nh=GH(xx+d1,yy+d2);if(nh<mh){mh=nh;bx=xx+d1;by=yy+d2}}if(bx!==xx||by!==yy)h[I(bx,by)]+=.008*ff;break}case'smooth':{let s=0,c=0;for(let d2=-1;d2<=1;d2++)for(let d1=-1;d1<=1;d1++){if(!d1&&!d2)continue;s+=GH(xx+d1,yy+d2);c++}if(c>0)h[I(xx,yy)]+=(s/c-h[I(xx,yy)])*.2*ff;break}case'flat':{let s=0,c=0;for(let d2=-2;d2<=2;d2++)for(let d1=-2;d1<=2;d1++){s+=GH(xx+d1,yy+d2);c++}let avg=s/c;h[I(xx,yy)]+=(avg-h[I(xx,yy)])*.25*ff;break}}}if(AT==='crater'){for(let dy=-R*2;dy<=R*2;dy++)for(let dx=-R*2;dx<=R*2;dx++){let d=Math.sqrt(dx*dx+dy*dy);if(d>R*2)continue;let xx=gx+dx,yy=gy+dy;if(xx<0||xx>=S||yy<0||yy>=S)continue;h[I(xx,yy)]-=.12*Math.exp(-d*d/(R*R*2));if(d>R*.5&&d<R*1.5)h[I(xx,yy)]+=.06*Math.sin((d-R*.5)/R*Math.PI)}AT='raise';document.querySelectorAll('.tool-btn').forEach(b=>b.classList.remove('active'));document.querySelector('[data-t="raise"]').classList.add('active')}}
cn.addEventListener('mousedown',e=>{MD=true;LLX=e.clientX;LLY=e.clientY;PT(e.clientX,e.clientY);SU()});
cn.addEventListener('mousemove',e=>{if(!MD)return;let steps=Math.max(Math.abs(e.clientX-LLX),Math.abs(e.clientY-LLY));for(let s=0;s<=steps;s++){let t2=steps>0?s/steps:0;PT((LLX+(e.clientX-LLX)*t2)|0,(LLY+(e.clientY-LLY)*t2)|0)}LLX=e.clientX;LLY=e.clientY;SU()});
window.addEventListener('mouseup',()=>{if(MD){MD=false;FU()}});
cn.addEventListener('wheel',e=>{e.preventDefault();ZO=Math.max(.3,Math.min(3,ZO-e.deltaY*.002))},{passive:false});
cn.addEventListener('touchstart',e=>{e.preventDefault();let t=e.touches[0];MD=true;LLX=t.clientX;LLY=t.clientY;PT(t.clientX,t.clientY);SU()},{passive:false});
cn.addEventListener('touchmove',e=>{e.preventDefault();if(!MD||e.touches.length>1)return;let t=e.touches[0],steps=Math.max(Math.abs(t.clientX-LLX),Math.abs(t.clientY-LLY));for(let s=0;s<=steps;s++){let p2=steps>0?s/steps:0;PT((LLX+(t.clientX-LLX)*p2)|0,(LLY+(t.clientY-LLY)*p2)|0)}LLX=t.clientX;LLY=t.clientY;SU()},{passive:false});
cn.addEventListener('touchend',e=>{e.preventDefault();if(MD){MD=false;FU()}},{passive:false});
window.addEventListener('keydown',e=>{let km={1:'raise',2:'lower',3:'water',4:'forest',5:'erode',6:'smooth',7:'flat',8:'crater'};if(km[e.key]){let b=document.querySelector('[data-t="'+km[e.key]+'"]');if(b)b.click()}if(e.key=='['){let s=document.getElementById('str');s.value=Math.max(1,+s.value-1);s.dispatchEvent(new Event('input'))}if(e.key==']'){let s=document.getElementById('str');s.value=Math.min(10,+s.value+1);s.dispatchEvent(new Event('input'))}});
// Loop
let PEN=false,TK=0;
function SU(){if(!PEN){PEN=true;requestAnimationFrame(()=>{FU();PEN=false})}}
function FU(){FLOW();ERODE(2,.004);GROW(1);SPREAD(1);RENDER();US()}
function US(){let es=0,wc=0,fs2=0,cnt=0;for(let i=0;i<S*S;i++){es+=h[i];if(w[i]>.05)wc++;fs2+=f[i];cnt++}let ae=(es/cnt*100)|0,wp=(wc/cnt*100)|0,fp=(fs2/cnt*100)|0,sc=Math.max(0,Math.min(100,(ae+50)*.4+wp*.5+fp*1.5))|0;document.getElementById('se').textContent=ae+'%';document.getElementById('sw').textContent=wp+'%';document.getElementById('sf').textContent=fp+'%';document.getElementById('ss').textContent=sc}
function GL(){TK++;if(!MD&&TK%5===0){FLOW();GROW(1);if(TK%12===0){ERODE(1,.002);SPREAD(1)}RENDER();US()}if(MD)RENDER();requestAnimationFrame(GL)}
window.addEventListener('resize',()=>RS());
requestAnimationFrame(GL);FU();
document.getElementById('info').textContent='Click & drag  1-8 tools  [/] size';
</script>
</body>
</html>"""

os.makedirs(r'C:\Users\Asus\terrafold', exist_ok=True)
with open(r'C:\Users\Asus\terrafold\index.html', 'w', encoding='utf-8') as f:
    f.write(HTML)
print(f'Wrote {len(HTML)} bytes')