#pragma once
// Main-loop-owned control state. Audio worker only reads published snapshots.
bool runtimeArmed=false,dictationReady=false,submitReady=false,permitted=false;
uint32_t pressSeq=0,segmentSeq=0,comboSeq=0;
bool suppressSend=false,submitArmed=false;
String controlMode="IDLE";
struct PendingControl {String base;uint32_t seq=0,at=0,sent=0;uint8_t attempts=0;};
PendingControl controls[9];
void clearControls(){for(auto &c:controls)c=PendingControl();}
void closeRuntime(){
  permitted=false;dictationReady=false;submitReady=false;runtimeArmed=false;submitArmed=false;
  pressSeq=segmentSeq=comboSeq=0;suppressSend=false;controlMode="IDLE";clearControls();
}
void controlEvent(const char *action){
  if(!authenticated||!versionOK)return;
  if(seq>=0xffffffef){revoke();return;}
  uint32_t next=++seq;
  String a(action);
  if(a=="DOWN"){pressSeq=segmentSeq=next;comboSeq=0;suppressSend=false;}
  if(a=="COMBO_BEGIN"){comboSeq=next;suppressSend=true;}
  if(a=="COMBO_RESUME")segmentSeq=next;
  bool management=record.state!=PAIRED||bleOn;
  if(management&&a!="STATE")return;
  String base="X1|E|"+device+"|"+dn+"|"+hn+"|"+String(next)+"|"+a+"|"+
    String(management?0:(ptt&&(controlMode=="TALK"||controlMode=="COMBO")))+"|"+String(management?0:submit)+"|"+(management?String("IDLE"):controlMode)+"|"+
    String(management?0:pressSeq)+"|"+String(management?0:segmentSeq)+"|"+String(management?0:comboSeq)+"|"+String(management?0:suppressSend);
  int slot=-1;
  if(a=="STATE")slot=8;else for(int i=0;i<8;i++)if(!controls[i].seq){slot=i;break;}
  // Ambiguous input is never replayed after reconnect. STATE may repair holds.
  if(slot<0){permitted=false;revoke();return;}
  controls[slot]={base,next,millis(),millis(),1};send(sign(base),hostIP,hostPort);
}
bool decimal(const String &s,uint32_t &out){
  if(!s.length()||s.length()>10)return false;char *end=nullptr;unsigned long n=strtoul(s.c_str(),&end,10);
  if(*end||n>=0xfffffff0||String(n)!=s)return false;out=n;return true;
}
void controlAck(const String &base,uint32_t now){
  for(auto &c:controls)if(c.seq&&now-c.at<1200&&base==String("X1|A")+c.base.substring(4)){lastAck=now;c=PendingControl();return;}
}
void controlPermission(String f[],const String &packet,int cut,uint32_t now){
  uint32_t hs,ss,p,s;
  if(!decimal(f[5],hs)||!hs||!decimal(f[6],ss)||ss>seq||!decimal(f[7],p)||!decimal(f[8],s)||p!=pressSeq||s!=segmentSeq||
      (f[9]!="0"&&f[9]!="1")||(f[10]!="0"&&f[10]!="1")||(f[11]!="0"&&f[11]!="1"))return;
  if(hs>hseq){
    hseq=hs;lastG=now;
    bool active=record.state==PAIRED&&!bleOn;
    dictationReady=active&&f[9]=="1";submitReady=active&&f[10]=="1";
    permitted=active&&dictationReady&&f[11]=="1"&&ss>=segmentSeq&&ptt&&!submit&&controlMode=="TALK";
    if(!dictationReady&&pressSeq&&ptt){runtimeArmed=false;permitted=false;}
  }
  send(sign("X1|K"+packet.substring(4,cut)),hostIP,hostPort);
}
void controlTick(uint32_t now){
  if(lastG&&now-lastG>=1200){uint32_t age=now-lastG,eventSeq=seq;revoke();Serial.printf("[SESSION] permission_timeout age_ms=%lu seq=%lu\n",(unsigned long)age,(unsigned long)eventSeq);return;}
  for(auto &c:controls)if(c.seq){
    if(now-c.at>=1200){uint32_t age=now-c.at,eventSeq=c.seq;uint8_t attempts=c.attempts;revoke();Serial.printf("[SESSION] control_ack_timeout age_ms=%lu seq=%lu attempts=%u\n",(unsigned long)age,(unsigned long)eventSeq,attempts);return;}
    if(now-c.sent>=180&&c.attempts<5){c.attempts++;c.sent=now;send(sign(c.base),hostIP,hostPort);}
  }
  if(now-lastState>=400){lastState=now;controlEvent("STATE");}
}
