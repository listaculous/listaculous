-- '' = full access; 'aws_setup' = single-use key for the AWS CLI setup script (can only fetch the
-- script and connect one AWS account; deleted by the server once it has).
ALTER TABLE api_keys ADD COLUMN scope TEXT NOT NULL DEFAULT '';
