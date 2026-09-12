use reqwest::blocking::Client;

fn main() {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    let supabase_url = "https://ynpdoctqgwrehcvdbsyy.supabase.co";
    let api_key = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InlucGRvY3RxZ3dyZWhjdmRic3l5Iiwicm9sZSI6InNlcnZpY2Vfcm9sZSIsImlhdCI6MTc4ODQ2MDA1NiwiZXhwIjoyMTA0MDM2MDU2fQ.YwbJTErt9Z0qqae1_CvtLDBTIQMuuEJLprc0cxGXANI";
    let username = "andreah";
    let password = "123Sam@1";

    let base_url = supabase_url.trim_end_matches('/');
    let encoded_username = username.replace('%', "%25").replace('=', "%3D").replace('&', "%26");
    let url = format!("{}/rest/v1/users?name=eq.{}&select=id,name,credential_hash,status",
        base_url, encoded_username);

    println!("=== Testing Supabase Login ===");
    println!("URL: {}", url);
    println!("");

    let response = client.get(&url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .unwrap();

    println!("Status: {}", response.status());

    let body = response.text().unwrap();
    println!("Body: {}", body);
    println!("");

    // Parse the response
    let users: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    if let Some(user) = users.into_iter().next() {
        let stored_hash = user.get("credential_hash").and_then(|v| v.as_str()).unwrap();
        println!("Stored hash: {}", stored_hash);
        println!("Password to verify: {}", password);
        println!("");
        println!("User found in Supabase!");
        println!("  id: {}", user.get("id").and_then(|v| v.as_str()).unwrap());
        println!("  name: {}", user.get("name").and_then(|v| v.as_str()).unwrap());
        println!("  status: {}", user.get("status").and_then(|v| v.as_str()).unwrap());
        println!("");
        println!("Now verifying password locally using argon2...");

        // Test password verification
        use argon2::{
            password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
            Argon2,
        };

        let parsed_hash = PasswordHash::new(stored_hash).unwrap();
        let result = Argon2::default().verify_password(password.as_bytes(), &parsed_hash);
        match result {
            Ok(()) => println!("✅ Password VERIFIED successfully!"),
            Err(e) => println!("❌ Password verification FAILED: {:?}", e),
        }
    } else {
        println!("❌ User not found!");
    }
}
