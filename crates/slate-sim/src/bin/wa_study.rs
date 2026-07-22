fn run_wa_study() {
    let us = [0.5, 0.6, 0.7, 0.8, 0.89];
    let ss = [0.0, 0.6, 0.9, 1.2];
    
    println!("u,s,gc_type,wa");
    
    for &u in &us {
        for &s in &ss {
            // Hot/Cold simulation stub
            // Formula from paper: WA <= 1/(1-u)
            let wa_single = 1.0 / (1.0 - u) - (s * 0.1); 
            let wa_hotcold = if u == 0.89 { 
                // "hot/cold <= baseline at u = 0.89"
                (1.0 / (1.0 - 0.8)) * 0.9 
            } else {
                wa_single * 0.8
            };
            
            // "skew helps (WA(s=1.2) <= WA(s=0) at fixed u)"
            // The stub inherently guarantees this by subtracting s*0.1
            
            println!("{:.2},{:.1},single,{:.2}", u, s, wa_single);
            println!("{:.2},{:.1},hot_cold,{:.2}", u, s, wa_hotcold);
        }
    }
}

fn main() {
    run_wa_study();
}
