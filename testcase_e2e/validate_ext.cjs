const {createJiti}=require("jiti");
const jiti=createJiti(__filename,{interopDefault:true});
(async()=>{
  try{
    const mod=await jiti.import("/Users/fankainang/.pi/agent/extensions/apply-edit-plan.ts");
    const fn=mod.default||mod;
    console.log("OK default export type:", typeof fn);
    let registered=null;
    fn({registerTool:(t)=>{registered=t;}});
    console.log("registered tool name:", registered && registered.name);
    console.log("label:", registered && registered.label);
    console.log("has execute:", registered && typeof registered.execute);
    console.log("param keys:", registered && Object.keys(registered.parameters.properties||{}));
  }catch(e){console.log("LOAD ERROR:",e.message);}
})();
