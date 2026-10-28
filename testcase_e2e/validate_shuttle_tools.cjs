const {createJiti}=require("jiti");
const jiti=createJiti(__filename,{interopDefault:true});
(async()=>{
  try{
    const mod=await jiti.import("/Users/fankainang/.pi/agent/extensions/shuttle-tools.ts");
    const fn=mod.default||mod;
    console.log("OK default export type:", typeof fn);
    const tools=[];
    fn({registerTool:(t)=>{tools.push(t);}});
    console.log("tool count:", tools.length);
    for(const t of tools){
      console.log("-", t.name, "| exec:", typeof t.execute, "| params:", Object.keys((t.parameters&&t.parameters.properties)||{}).join(","));
    }
  }catch(e){console.log("LOAD ERROR:",e.message);}
})();
