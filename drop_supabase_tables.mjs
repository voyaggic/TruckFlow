import { createClient } from '@supabase/supabase-js';

const supabaseUrl = 'https://ynpdoctqgwrhehcvdbsyy.supabase.co';
const serviceRoleKey = 'sbp_fce6bae6f9011ba0e631b8e074b4dd8c8b9dc80a';

const supabase = createClient(supabaseUrl, serviceRoleKey, {
  db: { schema: 'public' }
});

async function dropAllTables() {
  console.log('Connecting to Supabase...');

  try {
    const { data: tables, error } = await supabase
      .from('pg_tables')
      .select('tablename')
      .eq('schemaname', 'public');

    if (error) {
      console.error('Error fetching tables:', error);
      return;
    }

    console.log(`Found ${tables.length} tables to delete`);

    for (const { tablename } of tables) {
      console.log(`Dropping table: ${tablename}`);
      const { error: dropError } = await supabase.rpc('drop_table', { table_name: tablename });
      if (dropError) {
        console.error(`Failed to drop ${tablename}:`, dropError);
      } else {
        console.log(`Dropped: ${tablename}`);
      }
    }

    console.log('Done!');
  } catch (err) {
    console.error('Unexpected error:', err);
  }
}

dropAllTables();
