// XIAO ESP32S3 Sense, Arduino ESP32 3.3.12. Authenticated X1 / P8AU firmware.
#include <Arduino.h>
#if !ARDUINO_USB_CDC_ON_BOOT || !ARDUINO_USB_MODE
#error "Build with USBMode=hwcdc,CDCOnBoot=default so Serial uses the USB COM port."
#endif
#include <WiFi.h>
#include <WiFiUdp.h>
#include <WiFiProv.h>
#include <network_provisioning/manager.h>
#include <esp_wifi.h>
#include <esp_system.h>
#include <esp_mac.h>
#include <nvs.h>
#include <cJSON.h>
#include <mbedtls/md.h>

static const char POP[]="24681357"; // Accepted personal-use shared PoP, not device identity.
static const int PTT=1, SUBMIT=2, LED=21; // LED active-low; polarity requires User UAT.
enum SavedState:uint32_t { EMPTY=0, PREPARED=1, WAIT_FINAL=2, PAIRED=3, QUARANTINED=4 };
struct PairRecord {uint32_t schema; SavedState state; uint8_t key[32];char tx[33];wifi_config_t wifi;};
PairRecord record={};
SemaphoreHandle_t mutex;
struct Guard {
  bool locked;
  explicit Guard(TickType_t wait=portMAX_DELAY):locked(mutex&&xSemaphoreTakeRecursive(mutex,wait)==pdTRUE){}
  ~Guard(){if(locked)xSemaphoreGiveRecursive(mutex);}
  Guard(const Guard&)=delete;Guard& operator=(const Guard&)=delete;
};
WiFiUDP udp;
String device, dn, hn, stage="search";
IPAddress hostIP;
uint16_t hostPort=3335;
uint32_t ownerSession=0,identifyAt=0,startedAt=0,lastBeacon=0,lastOnline=0,lastState=0,seq=0,lastAck=0,lastG=0,hseq=0,comboAt=0,stopAt=0,rebootAt=0,lastWifiRetry=0,retryMs=5000;
bool identified=false,sawReleased=false,sawPressed=false,authenticated=false,versionOK=false,bleOn=true,udpOn=false,finalizedThisBoot=false,fatal=false;
bool rawPtt=false,rawSubmit=false,ptt=false,submit=false,comboLatched=false;
uint32_t debounceAt=0,hostRetryAt=0,hostRetryMs=5000;
struct NetEvent {int id;int reason;};
QueueHandle_t wifiEvents;
volatile bool wifiEventOverflow=false;
struct Event {uint32_t id;String code,stage,reason;};
Event events[16];uint32_t eventId=0;size_t eventCount=0;bool eventsOverflow=false;String lastErrorCode;

String hexBytes(const uint8_t *b,size_t n){static const char h[]="0123456789abcdef";String s;s.reserve(n*2);for(size_t i=0;i<n;i++){s+=h[b[i]>>4];s+=h[b[i]&15];}return s;}
String randomHex(size_t n){uint8_t b[32];esp_fill_random(b,n);String s=hexBytes(b,n);memset(b,0,sizeof(b));return s;}
bool hexString(const String &s,size_t n){if(s.length()!=n)return false;for(char c:s)if(!((c>='0'&&c<='9')||(c>='a'&&c<='f')))return false;return true;}
String sign(const String &base){uint8_t tag[32];mbedtls_md_context_t ctx;mbedtls_md_init(&ctx);const auto *info=mbedtls_md_info_from_type(MBEDTLS_MD_SHA256);if(mbedtls_md_setup(&ctx,info,1)!=0){mbedtls_md_free(&ctx);return "";}mbedtls_md_hmac_starts(&ctx,record.key,32);mbedtls_md_hmac_update(&ctx,(const uint8_t*)base.c_str(),base.length());mbedtls_md_hmac_finish(&ctx,tag);mbedtls_md_free(&ctx);return base+"|"+hexBytes(tag,32);}
bool valid(const String &base,const String &tag){if(!hexString(tag,64))return false;String expected=sign(base);String t=expected.substring(expected.length()-64);uint8_t diff=0;for(int i=0;i<64;i++)diff|=tag[i]^t[i];return diff==0;}
void send(const String &packet,IPAddress ip,uint16_t port){if(packet.isEmpty())return;udp.beginPacket(ip,port);udp.print(packet);udp.endPacket();}
void error(const char *code,const char *where,const char *reason){
  stage="error";lastErrorCode=code;if(eventCount==16){eventsOverflow=true;for(size_t i=1;i<16;i++)events[i-1]=events[i];eventCount=15;}
  events[eventCount++]={++eventId,String(code),String(where),String(reason)};
  Serial.printf("[ERROR] %s/%s/%s\n",code,where,reason);
}
bool saveRecord(SavedState state){
  PairRecord next=record;next.schema=1;next.state=state;
  if(esp_wifi_get_config(WIFI_IF_STA,&next.wifi)!=ESP_OK)return false;
  nvs_handle_t h;if(nvs_open("gptw-x1",NVS_READWRITE,&h)!=ESP_OK)return false;
  esp_err_t e=nvs_set_blob(h,"pair",&next,sizeof(next));if(e==ESP_OK)e=nvs_commit(h);nvs_close(h);
  if(e!=ESP_OK)return false;record=next;return true;
}
bool resetMarker(bool set){nvs_handle_t h;if(nvs_open("gptw-reset",NVS_READWRITE,&h)!=ESP_OK)return false;esp_err_t e=set?nvs_set_u8(h,"pending",1):nvs_erase_key(h,"pending");if(!set&&e==ESP_ERR_NVS_NOT_FOUND)e=ESP_OK;if(e==ESP_OK)e=nvs_commit(h);nvs_close(h);return e==ESP_OK;}
bool cleanReset(){
  // RESETTING is in a separate namespace and is removed only after every clear succeeds.
  nvs_handle_t h;if(nvs_open("gptw-x1",NVS_READWRITE,&h)!=ESP_OK)return false;
  esp_err_t e=nvs_erase_all(h);if(e==ESP_OK)e=nvs_commit(h);nvs_close(h);if(e!=ESP_OK)return false;
  if(esp_wifi_restore()!=ESP_OK)return false;
  memset(&record,0,sizeof(record));return resetMarker(false);
}
void revoke();
#include "controls.h"
#include "audio_session.h"
void revoke(){bool hadSession=authenticated||hn.length();closeRuntime();authenticated=false;versionOK=false;hn="";dn=randomHex(8);seq=0;hseq=0;lastAck=0;lastG=0;if(hadSession){hostRetryAt=millis()+hostRetryMs;hostRetryMs=min((uint32_t)30000,hostRetryMs+5000);}publishAudio();}
void physicalReset(){
  revoke();stage="resetting";
  if(!resetMarker(true)||!cleanReset()){fatal=true;error("HostError","reset","persist_failed");return;}
  rebootAt=millis()+500;
}
void onEvent(arduino_event_t *e){
  NetEvent event={0,0};
  if(e->event_id==ARDUINO_EVENT_WIFI_STA_GOT_IP)event.id=1;
  if(e->event_id==ARDUINO_EVENT_WIFI_STA_DISCONNECTED){event.reason=e->event_info.wifi_sta_disconnected.reason;event.id=2;}
  if(e->event_id==ARDUINO_EVENT_PROV_CRED_RECV)event.id=3;
  if(e->event_id==ARDUINO_EVENT_PROV_END)event.id=4;
  if(event.id&&wifiEvents&&xQueueSend(wifiEvents,&event,0)!=pdTRUE)wifiEventOverflow=true;
}
const char *text(cJSON *o,const char *name){const cJSON *v=cJSON_GetObjectItemCaseSensitive(o,name);return cJSON_IsString(v)?v->valuestring:nullptr;}
bool only(cJSON *o,const char *allowed){for(cJSON *i=o?o->child:nullptr;i;i=i->next){String needle=String("|")+i->string+"|";if(!strstr(allowed,needle.c_str()))return false;}return true;}
bool nonceArgs(cJSON *a){const char *d=text(a,"deviceNonce"),*h=text(a,"hostNonce");return only(a,"|deviceNonce||hostNonce|")&&d&&h&&dn==d&&hn==h&&authenticated&&versionOK&&WiFi.status()==WL_CONNECTED;}
const char *phase(){if(record.state==PAIRED)return finalizedThisBoot?"FINALIZED":"PAIRED";if(record.state==WAIT_FINAL)return "WAIT_FINAL";if(record.state==PREPARED)return "PREPARED";return "CANDIDATE";}
esp_err_t setupEndpoint(uint32_t session,const uint8_t *in,ssize_t len,uint8_t **out,ssize_t *outLen,void*){
  Serial.println("[SETUP] enter");
  Guard guard(pdMS_TO_TICKS(1500));
  if(!guard.locked){Serial.println("[SETUP] lock_timeout");return ESP_ERR_TIMEOUT;}
  Serial.println("[SETUP] lock_acquired");
  if(!in||len<=0||len>2048){Serial.println("[SETUP] invalid_bounds");return ESP_ERR_INVALID_ARG;}
  cJSON *q=cJSON_ParseWithLength((const char*)in,len);if(!q){Serial.println("[SETUP] invalid_json");return ESP_ERR_INVALID_ARG;}
  cJSON *id=cJSON_GetObjectItemCaseSensitive(q,"id"),*v=cJSON_GetObjectItemCaseSensitive(q,"v"),*a=cJSON_GetObjectItemCaseSensitive(q,"args");
  const char *op=text(q,"op"),*tx=text(q,"tx");
  if(!only(q,"|v||id||op||tx||args|")||!cJSON_IsNumber(v)||v->valuedouble!=1||!cJSON_IsNumber(id)||id->valuedouble<1||id->valuedouble>0xfffffffe||id->valuedouble!=(uint32_t)id->valuedouble||!op||!cJSON_IsObject(a)){Serial.println("[SETUP] invalid_envelope");cJSON_Delete(q);return ESP_ERR_INVALID_ARG;}
  Serial.println("[SETUP] envelope_valid");
  if(strcmp(op,"HELLO")==0)Serial.println("[SETUP] command_HELLO");
  else if(strcmp(op,"IDENTIFY")==0)Serial.println("[SETUP] command_IDENTIFY");
  else if(strcmp(op,"STATUS")==0)Serial.println("[SETUP] command_STATUS");
  cJSON *r=cJSON_CreateObject(),*data=cJSON_CreateObject();String result="REJECTED";
  bool matchTx=record.tx[0]?tx&&String(tx)==record.tx:(!tx);
  if(!fatal&&String(op)=="HELLO"&&only(a,"")){
    // HELLO never enrolls an already paired device or grants runtime permission.
    result="OK";cJSON_AddNumberToObject(data,"appProtocol",1);cJSON_AddStringToObject(data,"audio","P8AU/1");cJSON_AddStringToObject(data,"device",device.c_str());cJSON_AddStringToObject(data,"firmwareVersion","0.4.0");
    cJSON *caps=cJSON_AddArrayToObject(data,"capabilities");for(const char *c:{"identify1","commit1","combo1","gate1"})cJSON_AddItemToArray(caps,cJSON_CreateString(c));
  }else if(!fatal&&matchTx&&String(op)=="IDENTIFY"&&only(a,"")&&record.state==EMPTY){
    ownerSession=session;identified=false;sawReleased=false;sawPressed=false;identifyAt=millis();stage="identify";result="OK";
  }else if(!fatal&&String(op)=="STATUS"&&only(a,"")&&matchTx){result="OK";
  }else if(!fatal&&bleOn&&matchTx&&session==ownerSession&&record.state==EMPTY&&record.tx[0]&&stage=="error"&&lastErrorCode=="E08"&&String(op)=="WIFI_RETRY"&&only(a,"")&&WiFi.status()!=WL_CONNECTED){
    // Only an uncommitted provisioning session can replace failed credentials.
    revoke();
    if(network_prov_mgr_reset_wifi_sm_state_on_failure()==ESP_OK){stage="wifi";lastErrorCode="";eventCount=0;eventsOverflow=false;result="OK";}
  }else if(!fatal&&matchTx&&session==ownerSession&&String(op)=="BEGIN"&&only(a,"")&&identified&&record.state==EMPTY&&record.tx[0]==0){
    String t=randomHex(16);strlcpy(record.tx,t.c_str(),sizeof(record.tx));esp_fill_random(record.key,32);startedAt=millis();revoke();result="OK";cJSON_AddStringToObject(data,"key",hexBytes(record.key,32).c_str());
  }else if(!fatal&&matchTx&&session==ownerSession&&record.state==EMPTY&&String(op)=="HOST_CONFIG"&&only(a,"|controlPort||audioPort|")&&cJSON_GetNumberValue(cJSON_GetObjectItem(a,"controlPort"))==3335&&cJSON_GetNumberValue(cJSON_GetObjectItem(a,"audioPort"))==3333){
    if(WiFi.status()==WL_CONNECTED){revoke();startedAt=millis();stage="discovering";}
    result="OK";
  }else if(!fatal&&matchTx&&session==ownerSession&&record.tx[0]&&nonceArgs(a)){
    if(String(op)=="PREPARE"&&record.state==EMPTY&&saveRecord(PREPARED)){stage="preparing";result="DEVICE_PREPARED";}
    else if(String(op)=="COMMIT"&&record.state==PREPARED&&saveRecord(WAIT_FINAL)){stage="committing";result="DEVICE_SAVED";}
    else if(String(op)=="FINALIZE"&&record.state==WAIT_FINAL&&saveRecord(PAIRED)){finalizedThisBoot=true;stage="waiting_wifi_only";stopAt=millis()+1200;result="FINALIZED";}
  }else if(!fatal&&matchTx&&String(op)=="CANCEL"&&only(a,"")){
    // Commit/cancel races quarantine persistently before restarting in BLE mode.
    revoke();if(resetMarker(true)){result="CANCELED";rebootAt=millis()+1200;stage="canceled";}else{fatal=true;}
  }
  cJSON_AddBoolToObject(data,"eventsOverflow",eventsOverflow);eventsOverflow=false;
  cJSON_AddNumberToObject(r,"v",1);cJSON_AddNumberToObject(r,"id",id->valuedouble);
  if(record.tx[0])cJSON_AddStringToObject(r,"tx",record.tx);else cJSON_AddNullToObject(r,"tx");
  cJSON_AddStringToObject(r,"stage",stage.c_str());cJSON_AddStringToObject(r,"result",result.c_str());cJSON_AddItemToObject(r,"data",data);
  cJSON *es=cJSON_AddArrayToObject(r,"events");for(size_t i=0;i<eventCount;i++){cJSON *e=cJSON_CreateObject();cJSON_AddNumberToObject(e,"eventId",events[i].id);cJSON_AddStringToObject(e,"code",events[i].code.c_str());cJSON_AddStringToObject(e,"stage",events[i].stage.c_str());cJSON_AddStringToObject(e,"reason",events[i].reason.c_str());cJSON_AddItemToArray(es,e);}
  eventCount=0;char *encoded=cJSON_PrintUnformatted(r);cJSON_Delete(q);cJSON_Delete(r);if(!encoded){Serial.println("[SETUP] response_alloc_failed");return ESP_ERR_NO_MEM;}
  *outLen=strlen(encoded);*out=(uint8_t*)encoded;Serial.println("[SETUP] response_ready");return ESP_OK;
}
void updateButtons(){
  uint32_t now=millis();bool p=digitalRead(PTT)==LOW,s=digitalRead(SUBMIT)==LOW;
  bool oldP=ptt,oldS=submit;
  // Raw chord closes worker gate before debounce and independently of Host.
  if(p&&s){
    permitted=false;
    if(!comboAt)comboAt=now;
    comboLatched=true;
    if(now-comboAt>=5000){
      controlMode="RESETTING";suppressSend=true;controlEvent("RESET");publishAudio();physicalReset();comboAt=0;return;
    }
  }else comboAt=0;
  if(!p||s)permitted=false;
  if(p!=rawPtt||s!=rawSubmit){rawPtt=p;rawSubmit=s;debounceAt=now;}
  if(now-debounceAt<25)return;ptt=p;submit=s;
  if(stage=="identify"){
    if(now-identifyAt>=30000){identified=false;error("E07","identify","timeout");}
    else if(!p&&!s&&!sawPressed)sawReleased=true;
    else if(sawReleased&&p&&!s&&!comboLatched)sawPressed=true;
    if(sawPressed&&!p&&!s&&!comboLatched){identified=true;stage="wifi";}
  }
  bool active=authenticated&&versionOK&&record.state==PAIRED&&!bleOn;
  if(active){
    if(p&&s&&controlMode!="COMBO"&&controlMode!="FINISH_NO_SEND"){
      controlMode="COMBO";suppressSend=true;submitArmed=false;controlEvent("COMBO_BEGIN");
    }else if(controlMode=="COMBO"&&!p){
      controlMode="FINISH_NO_SEND";permitted=false;runtimeArmed=false;submitArmed=false;controlEvent("COMBO_END");
    }else if(controlMode=="COMBO"&&p&&!s){
      controlMode=pressSeq?"TALK":"IDLE";permitted=false;controlEvent(pressSeq?"COMBO_RESUME":"STATE");
    }else if(!p&&oldP&&controlMode=="TALK"){
      controlMode=suppressSend?"FINISH_NO_SEND":"IDLE";permitted=false;controlEvent("UP");
    }else if(p&&!oldP&&!s&&runtimeArmed){
      if(dictationReady&&now-lastG<1200){controlMode="TALK";controlEvent("DOWN");}
      else runtimeArmed=false;
    }else if(s&&!oldS&&!p&&submitArmed&&submitReady&&!comboLatched){
      // A fresh independent press is legal after a completed chord. No held Submit.
      controlMode="IDLE";suppressSend=false;comboSeq=0;controlEvent("SUBMIT");submitArmed=false;
    }
    if(!p&&!s){runtimeArmed=true;submitArmed=true;}
  }
  if(!p&&!s)comboLatched=false;
}

void updateWifi(){
  if(wifiEventOverflow){wifiEventOverflow=false;error("HostError","wifi_events","overflow");}
  NetEvent event={0,0};
  for(int budget=0;budget<16&&xQueueReceive(wifiEvents,&event,0)==pdTRUE;budget++){
  int e=event.id;
  if(e==4){bleOn=false;stage="waiting_wifi_only";}
  if(e==3){startedAt=millis();stage="connecting";}
  if(e==1){if(!udpOn)udpOn=udp.begin(3334);startedAt=millis();lastAck=0;retryMs=5000;if(record.state!=PAIRED)stage="discovering";}
  if(e==2){revoke();Serial.printf("[SESSION] wifi_disconnected reason=%d\n",event.reason);if(bleOn&&record.tx[0])error("E08","wifi_auth",event.reason==WIFI_REASON_AUTH_FAIL?"incorrect_password":"ap_unavailable");}
  }
  if(WiFi.status()!=WL_CONNECTED){
    if(record.state==PAIRED&&millis()-lastWifiRetry>=retryMs){lastWifiRetry=millis();WiFi.reconnect();retryMs=min((uint32_t)30000,retryMs+5000);}
    if(bleOn&&stage=="connecting"&&millis()-startedAt>30000)error("E08","dhcp","timeout");return;
  }
  if(record.tx[0]&&!authenticated&&stage=="discovering"&&millis()-startedAt>20000)error("E09","host_discovery","host_not_found");
  if(record.tx[0]&&stage=="authenticating"&&millis()-startedAt>12000){error("E12","host_auth","timeout");revoke();}
}
void updateNetwork(){
  if(!udpOn||WiFi.status()!=WL_CONNECTED||!record.tx[0]||fatal)return;
  uint32_t now=millis();
  if(authenticated&&lastAck&&now-lastAck>2000){uint32_t age=now-lastAck;revoke();startedAt=now;Serial.printf("[SESSION] heartbeat_timeout ack_age_ms=%lu\n",(unsigned long)age);}
  if(!authenticated&&hn.length()&&now-startedAt>12000){revoke();startedAt=now;}
  if(!authenticated&&(!hostRetryAt||(int32_t)(now-hostRetryAt)>=0)&&now-lastBeacon>=1200){lastBeacon=now;IPAddress broadcast=~(uint32_t)WiFi.subnetMask()|(uint32_t)WiFi.localIP();send("P6|B|"+device+"|"+dn,broadcast,3335);}
  for(int budget=0;budget<16;budget++){
    int n=udp.parsePacket();if(!n)break;IPAddress ip=udp.remoteIP();uint16_t port=udp.remotePort();char b[513];int read=udp.read(b,512);if(n>512||read!=n||port!=3335)continue;b[n]=0;String packet(b);
    String f[16];int count=0,start=0;for(int i=0;i<=n;i++)if(i==n||b[i]=='|'){if(count>=16){count=0;break;}f[count++]=packet.substring(start,i);start=i+1;}
    if(count<6||f[2]!=device||f[3]!=dn)continue;int cut=packet.lastIndexOf('|');if(!valid(packet.substring(0,cut),f[count-1])){if(bleOn)error("E12","host_auth","proof_failed");continue;}
    if(f[0]=="P6"&&f[1]=="H"&&count==6&&hexString(f[4],16)&&!authenticated){
      if(hn.length()&&(hn!=f[4]||ip!=hostIP))continue;hostIP=ip;hostPort=port;hn=f[4];stage=record.state==PAIRED?"reconnecting":"authenticating";startedAt=now;send(sign("P6|P|"+device+"|"+dn+"|"+hn),hostIP,hostPort);continue;
    }
    if(ip!=hostIP||f[4]!=hn)continue;
    if(f[0]=="P6"&&f[1]=="A"&&count==6&&!authenticated){authenticated=true;lastAck=now;hostRetryMs=5000;hostRetryAt=0;}
    if(!authenticated)continue;
    if(f[0]=="X1"&&f[1]=="R"&&count==12&&f[5]==record.tx&&f[6]=="1"&&f[7]=="1"&&f[8]=="3335"&&f[9]=="3333"&&f[10]==phase()){versionOK=true;}
    if(f[0]=="X1"&&f[1]=="A"&&count==15){controlAck(packet.substring(0,cut),now);}
    if(f[0]=="X1"&&f[1]=="G"&&count==13&&versionOK){
      controlPermission(f,packet,cut,now);
    }
  }
  if(!authenticated)return;
  if(now-lastOnline>=400){lastOnline=now;send(sign("P6|O|"+device+"|"+dn+"|"+hn+"|"+(bleOn?"B":"W")),hostIP,hostPort);send(sign("X1|V|"+device+"|"+dn+"|"+hn+"|"+record.tx+"|1|1|3335|3333|"+phase()),hostIP,hostPort);}
  if(!versionOK)return;
  controlTick(now);
}
void setup(){
  Serial.begin(115200);
  // USB logging is best effort: an unopened/full host CDC reader must never
  // stall control heartbeats or raw-button cancellation. Drop on backpressure.
  Serial.setTxTimeoutMs(0);
  pinMode(PTT,INPUT_PULLUP);pinMode(SUBMIT,INPUT_PULLUP);pinMode(LED,OUTPUT);digitalWrite(LED,HIGH);
  mutex=xSemaphoreCreateRecursiveMutex();wifiEvents=xQueueCreate(16,sizeof(NetEvent));if(!mutex||!wifiEvents){fatal=true;return;}
  WiFi.onEvent(onEvent);WiFi.mode(WIFI_STA);WiFi.setSleep(false);
  nvs_handle_t h;uint8_t resetting=0;esp_err_t e=nvs_open("gptw-reset",NVS_READWRITE,&h);if(e!=ESP_OK){fatal=true;return;}e=nvs_get_u8(h,"pending",&resetting);nvs_close(h);if(e!=ESP_OK&&e!=ESP_ERR_NVS_NOT_FOUND){fatal=true;return;}
  if(resetting&&!cleanReset()){fatal=true;return;}
  if(nvs_open("gptw-x1",NVS_READWRITE,&h)!=ESP_OK){fatal=true;return;}size_t size=sizeof(record);e=nvs_get_blob(h,"pair",&record,&size);nvs_close(h);
  if(e!=ESP_ERR_NVS_NOT_FOUND&&(e!=ESP_OK||size!=sizeof(record)||record.schema!=1)){fatal=true;return;}
  // Read the stable STA address without depending on Arduino netif startup.
  uint8_t staMac[6]={};esp_err_t macResult=esp_read_mac(staMac,ESP_MAC_WIFI_STA);
  uint8_t anyByte=0;for(uint8_t b:staMac)anyByte|=b;
  if(macResult!=ESP_OK||!anyByte||(staMac[0]&1)){fatal=true;error("HostError","boot","invalid_sta_mac");return;}
  device=hexBytes(staMac,sizeof(staMac));device.toUpperCase();revoke();
  if(record.state==PAIRED&&record.tx[32]==0&&hexString(record.tx,32)){
    bleOn=false;stage="reconnecting";esp_wifi_set_config(WIFI_IF_STA,&record.wifi);esp_wifi_connect();
  }else{
    if(!resetMarker(true)||!cleanReset()){fatal=true;return;}
    WiFiProv.initProvision(NETWORK_PROV_SCHEME_BLE,NETWORK_PROV_SCHEME_HANDLER_NONE,false);
    if(network_prov_mgr_endpoint_create("gptw-setup")!=ESP_OK||!WiFiProv.disableAutoStop(1200)){fatal=true;return;}
    String name="PROV_GPT_"+device.substring(6);
    WiFiProv.beginProvision(NETWORK_PROV_SCHEME_BLE,NETWORK_PROV_SCHEME_HANDLER_NONE,NETWORK_PROV_SECURITY_1,POP,name.c_str(),nullptr,nullptr,false);
    if(network_prov_mgr_endpoint_register("gptw-setup",setupEndpoint,nullptr)!=ESP_OK){fatal=true;return;}
    bleOn=true;stage="search";
    Serial.printf("[BLE] %s\n",name.c_str());
  }
  startAudio();
  Serial.printf("[BOOT] GPTW 0.4.0 %s (%s)\n",device.c_str(),bleOn?"BLE":"WIFI_ONLY");
}
void loop(){
  if(!mutex){delay(1000);return;}
  bool stopProvision=false,reboot=false,lit=false;
  {
    Guard guard;
    updateButtons();if(!fatal){updateWifi();updateNetwork();}
    AudioMetrics metrics;portENTER_CRITICAL(&audioMux);metrics=audioMetrics;portEXIT_CRITICAL(&audioMux);
    if(metrics.exhausted){revoke();portENTER_CRITICAL(&audioMux);audioMetrics.exhausted=false;portEXIT_CRITICAL(&audioMux);}
    publishAudio();
    if(stopAt&&(int32_t)(millis()-stopAt)>=0){stopAt=0;stopProvision=true;}
    if(rebootAt&&(int32_t)(millis()-rebootAt)>=0){rebootAt=0;reboot=true;}
    bool audioGate=false;portENTER_CRITICAL(&audioMux);audioGate=audioShared.gate&&audioMetrics.ready;portEXIT_CRITICAL(&audioMux);
    uint32_t now=millis();uint32_t reconnectTick=now%1500;
    lit=fatal?(now%200<100):comboAt?(now%160<80):stage=="identify"?(now%300<150):bleOn?(now%1000<200):audioGate?true:(authenticated&&versionOK)?(now%2000<80):(reconnectTick<100||(reconnectTick>=250&&reconnectTick<350));
  }
  // BLE shutdown can wait for callbacks. Never hold their shared-state lock
  // across shutdown, reboot, or the loop's scheduling delay.
  if(stopProvision)WiFiProv.endProvision();
  if(reboot)ESP.restart();
  static uint32_t lastMetrics=0;if(millis()-lastMetrics>=10000){lastMetrics=millis();printAudioStatus();}
  digitalWrite(LED,lit?LOW:HIGH);delay(5);
}
